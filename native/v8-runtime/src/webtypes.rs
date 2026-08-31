//! Sandbox-side web types: `Headers`, `Request`, `Response`, plus the
//! `TextEncoder`/`TextDecoder`/`URL`/`URLSearchParams` they depend on.
//!
//! The sandbox is bare V8 — it has none of these. This module installs them.
//!
//! # Why the classes are half native
//!
//! Each of the three serializable classes is a JS class extending a tiny
//! native `FunctionTemplate` "shell" whose instance template declares one
//! internal field. That field is the whole point: V8 routes any object with
//! internal fields to `ValueSerializer::WriteHostObject` off a map field read,
//! with `HasCustomHostObject()` left `false`. Enabling `HasCustomHostObject`
//! would make V8 call back into Rust for *every* plain object serialized —
//! a per-object tax on the bulk export path. workerd avoids it the same way
//! (`src/workerd/jsg/ser.c++`, `Serializer::HasCustomHostObject`).
//!
//! The internal field is deliberately left **empty**. It exists only to make
//! V8 route the object; nothing is ever written into it. That is not an
//! oversight — `rusty_v8`'s built-in snapshot callback reads every embedder
//! field as an *aligned pointer* and memcpy's out of it:
//!
//! ```cpp
//! // v8 crate, src/binding.cc — same shape from 130 through 147; 147 added
//! // the embedder type-tag argument, and still reads an aligned pointer.
//! InternalFieldData* embedder_field = static_cast<InternalFieldData*>(
//!     holder->GetAlignedPointerFromInternalField(
//!         index, v8::kEmbedderDataTypeTagDefault));
//! if (embedder_field == nullptr) return {nullptr, 0};
//! memcpy(payload, embedder_field, sizeof(*embedder_field));
//! ```
//!
//! An earlier version stored the type tag here as a `v8::Integer`. A Smi read
//! back as a pointer is non-null garbage, so any snapshot containing a live
//! instance segfaulted the process — `globalThis.r = new Response("x")` in
//! prefix code was enough. Leaving the field empty makes the callback see
//! `nullptr` and return cleanly. Storing a real pointer instead would mean
//! `unsafe`, a per-instance allocation, and a dependency on the layout of
//! `rusty_v8`'s own `InternalFieldData` — far worse.
//!
//! The type tag therefore lives in a **private-symbol property** stamped onto
//! every instance at construction (the shell constructors and
//! `make_instance`). Private symbols are unreachable from guest JS — no
//! reflection API surfaces them — so the tag cannot be forged, removed, or
//! redirected via `Symbol.hasInstance`, and reading it is a plain data read
//! that is legal inside the `DisallowJavascriptExecutionScope` V8 holds during
//! deserialization. `tag_of` reads that stamp and consults nothing else; the
//! class references `make_instance` wires prototypes from are captured once at
//! `install()` into private slots on the global object, so neither direction
//! depends on what guest code has done to `globalThis`.
//!
//! Everything above the shell — the actual spec behaviour — is JS, evaluated
//! once into the context. A JS subclass of a shell keeps the internal field,
//! which is what makes this split possible.
//!
//! There is no startup-snapshot coupling anymore: runtime snapshot creation
//! was removed (V8 14.x cannot create snapshots safely in a live
//! multi-isolate process), so the classes are installed fresh into every
//! context and no external-references table exists. If snapshots ever return
//! (e.g. via a public `IsolateGroup` API), the table must come back with
//! them: native callbacks cannot cross a snapshot without it being supplied
//! at **both** creation and restore.

use v8::MapFnTo;

use crate::webcodec::{TAG_HEADERS, TAG_REQUEST, TAG_RESPONSE};

/// Number of internal fields on a host-type instance.
///
/// The field itself is never written — it exists purely so V8 routes the object
/// to `WriteHostObject`. See the module docs.
pub const INTERNAL_FIELD_COUNT: usize = 1;

// ── Native shells ────────────────────────────────────────────────────────────

/// V8's `kEmbedderDataTypeTagDefault` (`v8-object.h`), which the crate does not
/// re-export. It is the tag `binding.cc` uses on both sides of the snapshot
/// callback, so it is the tag the field must be written with.
const EMBEDDER_DATA_TYPE_TAG_DEFAULT: u16 = 0;

/// Zero the shell instance's internal field.
///
/// The field's *value* is still never used — the type tag lives in a private
/// property (see `stamp_tag`). But the slot must be a real
/// external-pointer slot before V8's snapshot callback reads it. `rusty_v8`
/// documents `get_aligned_pointer_from_internal_field` as undefined behaviour
/// unless `SetAlignedPointerInInternalField` wrote the field first, and since
/// V8 14.7 that read decodes through the *tagged* external-pointer table rather
/// than loading a raw word.
///
/// Leaving the field at its default `undefined` therefore made
/// `SerializeInternalFields` hand V8 a garbage pointer, and `CreateBlob` died
/// with SIGBUS in `ReadOnlyPromotion::Promote` whenever a prefix snapshot
/// contained a live instance. On V8 13.0 the same untagged read happened to
/// yield null, which is why the never-write design worked there.
///
/// This is the identical write the crate's own `DeserializeInternalFields`
/// performs when it restores an empty payload (`binding.cc`).
fn zero_internal_field(args: &v8::FunctionCallbackArguments) {
    args.this().set_aligned_pointer_in_internal_field(
        0,
        std::ptr::null(),
        EMBEDDER_DATA_TYPE_TAG_DEFAULT,
    );
}

/// Private-symbol key holding a host-type instance's type tag.
///
/// Stamped at construction, read by `tag_of`. Private symbols are invisible to
/// every guest-reachable reflection API, so guest JS can neither forge nor
/// remove the stamp.
const TYPE_TAG_KEY: &str = "iso4::webTypeTag";

fn tag_key<'s>(scope: &mut v8::PinScope<'s, '_>) -> Option<v8::Local<'s, v8::Private>> {
    let name = v8::String::new(scope, TYPE_TAG_KEY)?;
    Some(v8::Private::for_api(scope, Some(name)))
}

/// Private-symbol key on the global object holding one captured class
/// reference (`iso4::webClass::Headers`, …). Written once by `install()`.
fn class_key<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    class_name: &str,
) -> Option<v8::Local<'s, v8::Private>> {
    let name = v8::String::new(scope, &format!("iso4::webClass::{class_name}"))?;
    Some(v8::Private::for_api(scope, Some(name)))
}

/// Stamp the type tag onto an instance. `set_private` defines a data property
/// directly — no JS runs, so this is legal on the deserialize path too.
fn stamp_tag(scope: &mut v8::PinScope, obj: v8::Local<v8::Object>, tag: u32) -> Option<()> {
    let key = tag_key(scope)?;
    let value = v8::Integer::new_from_unsigned(scope, tag);
    obj.set_private(scope, key, value.into()).map(|_| ())
}

/// The shells make their internal field readable and stamp the type tag.
///
/// They exist so that `super()` produces an object built from an instance
/// template with one internal field — which is what V8 dispatches on (see the
/// module docs on why the field carries no data) — and so that every
/// construction path, including JS subclasses, passes through the stamp.
fn headers_shell_ctor(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    zero_internal_field(&args);
    // A failed stamp (allocation failure interning the key) leaves an instance
    // that later refuses to serialize with a clear message — not a crash path.
    let _ = stamp_tag(scope, args.this(), TAG_HEADERS);
}

fn request_shell_ctor(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    zero_internal_field(&args);
    // A failed stamp (allocation failure interning the key) leaves an instance
    // that later refuses to serialize with a clear message — not a crash path.
    let _ = stamp_tag(scope, args.this(), TAG_REQUEST);
}

fn response_shell_ctor(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    zero_internal_field(&args);
    // A failed stamp (allocation failure interning the key) leaves an instance
    // that later refuses to serialize with a clear message — not a crash path.
    let _ = stamp_tag(scope, args.this(), TAG_RESPONSE);
}

// ── Installation ─────────────────────────────────────────────────────────────

/// Private-symbol key under which `install` stashes the body-stream factory
/// the web runtime returns (for hydrating streamed host bodies). Read by the
/// codec's rehydration path in `v8.rs`.
pub const BODY_STREAM_FACTORY_KEY: &str = "iso4::bodyStreamFactory";

/// Private-symbol key on the global object holding the frozen-clock cell —
/// the plain object whose `t` property backs every guest-visible clock
/// (`Date`, no-arg `Intl.DateTimeFormat` formatting, `Temporal.Now`). The
/// shims capture the cell in closures at install time; the Rust side advances
/// it through this key, so guest code can neither reach the cell nor observe
/// time passing while it executes.
const FROZEN_CLOCK_KEY: &str = "iso4::frozenClock";

fn clock_key<'s>(scope: &mut v8::PinScope<'s, '_>) -> Option<v8::Local<'s, v8::Private>> {
    let name = v8::String::new(scope, FROZEN_CLOCK_KEY)?;
    Some(v8::Private::for_api(scope, Some(name)))
}

/// Wall clock in whole ms since the epoch — the value the frozen clock
/// advances to. Truncation to ms is deliberate: the guest never sees sub-ms
/// resolution even at an advance boundary.
fn wall_now_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as f64)
        .unwrap_or(0.0)
}

/// Advance the current context's frozen clock to `max(current, wall now)`.
///
/// Called only where the runtime regains control from the socket — run entry
/// and each received frame in the poll loop — never while guest code is on
/// the stack. Monotone by construction; a context without the cell (a partly
/// failed install) is left alone.
pub fn advance_frozen_clock(scope: &mut v8::PinScope) {
    let context = scope.get_current_context();
    let global = context.global(scope);
    let Some(key) = clock_key(scope) else { return };
    let Some(cell) = global.get_private(scope, key) else {
        return;
    };
    let Ok(cell) = v8::Local::<v8::Object>::try_from(cell) else {
        return;
    };
    let Some(prop) = v8::String::new(scope, "t") else { return };
    let prev = cell
        .get(scope, prop.into())
        .and_then(|v| v.number_value(scope))
        .unwrap_or(0.0);
    let now = wall_now_ms();
    if now > prev {
        let value = v8::Number::new(scope, now);
        cell.set(scope, prop.into(), value.into());
    }
}

/// Install the web globals into the current context, without streamed-body
/// support (unit tests): the stream natives are `undefined` and a streamed
/// body read fails cleanly in JS.
pub fn install(scope: &mut v8::PinScope) -> Result<(), String> {
    let read = v8::undefined(scope).into();
    let cancel = v8::undefined(scope).into();
    install_with_streams(scope, read, cancel)
}

/// Install the web globals into the current context.
///
/// Called at context creation on every path — prefix validation and every
/// run — so prefix and postfix code see the same environment. `stream_read`
/// and `stream_cancel` are the native callbacks backing streamed host bodies
/// (built in `v8.rs` around the run's stream table).
pub fn install_with_streams(
    scope: &mut v8::PinScope,
    stream_read: v8::Local<v8::Value>,
    stream_cancel: v8::Local<v8::Value>,
) -> Result<(), String> {
    let mut factory_args = Vec::with_capacity(5);
    for (name, ctor) in [
        ("HeadersShell", headers_shell_ctor.map_fn_to()),
        ("RequestShell", request_shell_ctor.map_fn_to()),
        ("ResponseShell", response_shell_ctor.map_fn_to()),
    ] {
        let tmpl = v8::FunctionTemplate::builder_raw(ctor).build(scope);
        tmpl.instance_template(scope)
            .set_internal_field_count(INTERNAL_FIELD_COUNT);
        let class_name =
            v8::String::new(scope, name).ok_or_else(|| format!("intern {name} failed"))?;
        tmpl.set_class_name(class_name);
        let func = tmpl
            .get_function(scope)
            .ok_or_else(|| format!("get_function for {name} failed"))?;
        factory_args.push(v8::Local::<v8::Value>::from(func));
    }

    // Native URL parsing (url.rs). Passed to the factory like the shells and
    // never exposed on `globalThis`.
    for (name, callback) in [
        ("urlParse", crate::url::url_parse_callback.map_fn_to()),
        ("urlSet", crate::url::url_set_callback.map_fn_to()),
    ] {
        let func = v8::Function::builder_raw(callback)
            .build(scope)
            .ok_or_else(|| format!("create {name} failed"))?;
        factory_args.push(v8::Local::<v8::Value>::from(func));
    }

    // Streamed-body natives (or `undefined` in stream-less installs).
    factory_args.push(stream_read);
    factory_args.push(stream_cancel);

    // The frozen-clock cell (see FROZEN_CLOCK_KEY). Initialized to the wall
    // clock at context creation, so prefix evaluation runs on a frozen "now"
    // exactly like calls do.
    let clock_cell = v8::Object::new(scope);
    {
        let prop = v8::String::new(scope, "t")
            .ok_or_else(|| "intern frozen clock prop failed".to_string())?;
        let value = v8::Number::new(scope, wall_now_ms());
        clock_cell
            .set(scope, prop.into(), value.into())
            .ok_or_else(|| "init frozen clock failed".to_string())?;
    }
    factory_args.push(clock_cell.into());

    // The runtime source is an expression evaluating to a function; calling it
    // with the three shells, the two URL callbacks, the stream natives, and
    // the frozen-clock cell installs the classes and the clock shims. None of
    // the arguments are ever exposed on `globalThis`.
    let source = v8::String::new(scope, RUNTIME_JS)
        .ok_or_else(|| "intern web runtime source failed".to_string())?;
    let origin_name = v8::String::new(scope, "iso4:web")
        .ok_or_else(|| "intern web runtime filename failed".to_string())?;
    let origin = v8::ScriptOrigin::new(
        scope,
        origin_name.into(),
        0,
        0,
        false,
        0,
        None,
        false,
        false,
        false,
        None,
    );

    v8::tc_scope!(let tc, scope);
    let script = v8::Script::compile(tc, source, Some(&origin))
        .ok_or_else(|| exception_text(tc, "compile web runtime"))?;
    let factory = script
        .run(tc)
        .ok_or_else(|| exception_text(tc, "evaluate web runtime"))?;
    let factory: v8::Local<v8::Function> = factory
        .try_into()
        .map_err(|_| "web runtime did not evaluate to a function".to_string())?;

    let recv = v8::undefined(tc).into();
    let returned = factory
        .call(tc, recv, &factory_args)
        .ok_or_else(|| exception_text(tc, "install web runtime"))?;

    // The factory returns [Headers, Request, Response]. Keep each under a
    // private slot on the global object: `make_instance` wires prototypes from
    // these captured references, so deserialization no longer depends on the
    // guest-mutable `globalThis` properties. Private slots survive anything
    // guest code can do and are readable without running JS.
    let classes: v8::Local<v8::Array> = returned
        .try_into()
        .map_err(|_| "web runtime did not return its classes".to_string())?;
    let global = tc.get_current_context().global(tc);
    for (index, name) in ["Headers", "Request", "Response"].iter().enumerate() {
        let class = classes
            .get_index(tc, index as u32)
            .filter(|c| c.is_function())
            .ok_or_else(|| format!("web runtime did not return the {name} class"))?;
        let key =
            class_key(tc, name).ok_or_else(|| format!("intern {name} class key failed"))?;
        global
            .set_private(tc, key, class)
            .ok_or_else(|| format!("store {name} class reference failed"))?;
    }

    // The body-stream factory for hydrating streamed host bodies, stashed
    // under a private key like the classes.
    let factory_fn = classes
        .get_index(tc, 3)
        .filter(|c| c.is_function())
        .ok_or_else(|| "web runtime did not return the body-stream factory".to_string())?;
    let factory_name = v8::String::new(tc, BODY_STREAM_FACTORY_KEY)
        .ok_or_else(|| "intern body-stream factory key failed".to_string())?;
    let factory_key = v8::Private::for_api(tc, Some(factory_name));
    global
        .set_private(tc, factory_key, factory_fn)
        .ok_or_else(|| "store body-stream factory failed".to_string())?;

    // Keep the frozen-clock cell reachable for advance_frozen_clock. Private
    // slot, same reasoning as the classes above.
    let clock_key =
        clock_key(tc).ok_or_else(|| "intern frozen clock key failed".to_string())?;
    global
        .set_private(tc, clock_key, clock_cell.into())
        .ok_or_else(|| "store frozen clock cell failed".to_string())?;
    Ok(())
}

fn exception_text(
    tc: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
    what: &str,
) -> String {
    let detail = tc
        .exception()
        .and_then(|e| e.to_string(tc))
        .map(|s| s.to_rust_string_lossy(tc))
        .unwrap_or_else(|| "no exception".to_string());
    tc.reset();
    format!("{what}: {detail}")
}

// ── The JS runtime ───────────────────────────────────────────────────────────

/// Instance state lives in non-enumerable own properties with short names, so
/// the Rust codec can read it with a handful of `Object::Get` calls instead of
/// invoking a JS method per field. Kept in sync with `webcodec.rs`:
///
/// | class    | property | holds                                    |
/// | -------- | -------- | ---------------------------------------- |
/// | Headers  | `_l`     | flat array `[name, value, name, value…]` |
/// | Request  | `_u`     | url string                               |
/// | Request  | `_m`     | method string                            |
/// | Request  | `_h`     | `Headers` instance                       |
/// | Request  | `_b`     | `null` \| `Uint8Array` \| string          |
/// | Response | `_s`     | status number                            |
/// | Response | `_t`     | statusText string                        |
/// | Response | `_h`     | `Headers` instance                       |
/// | Response | `_b`     | `null` \| `Uint8Array` \| string          |
///
/// Startup cost: this source is compiled and evaluated into every fresh
/// context. Deno avoids that by precompiling its JS runtime into a build-time
/// V8 snapshot, and that stays on the table for static sources like this one
/// (what was removed was *runtime* snapshot creation of live prefix state
/// in a multi-isolate process). A build-time snapshot must carry the
/// external-references table for every native callback above (see module
/// docs); the nearer-term step is a process-wide code cache.
const RUNTIME_JS: &str = include_str!("webtypes.js");

// ── Adapter ──────────────────────────────────────────────────────────────────
//
// The ONLY code that knows how sandbox instance state is laid out. `webcodec`
// talks to these functions and never touches a property name.
//
// Replacing the sandbox runtime — hand-written JS today, possibly Deno's
// `deno_fetch`/`deno_url` extension crates or native Rust classes later — means
// reimplementing this section and nothing else. The wire format in
// `docs/protocol.md` §4.4 is independent of it.

/// Field views hold V8 locals rather than owned Rust values: the codec writes
/// them straight into the serializer, so nothing is copied on the way through.
pub struct HeadersView<'s> {
    /// Flat `[name, value, name, value, …]`.
    pub list: v8::Local<'s, v8::Array>,
}

pub struct RequestView<'s> {
    pub url: v8::Local<'s, v8::Value>,
    pub method: v8::Local<'s, v8::Value>,
    pub headers: v8::Local<'s, v8::Object>,
    pub body: v8::Local<'s, v8::Value>,
}

pub struct ResponseView<'s> {
    pub status: u32,
    pub status_text: v8::Local<'s, v8::Value>,
    pub headers: v8::Local<'s, v8::Object>,
    pub body: v8::Local<'s, v8::Value>,
}

fn get<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    obj: v8::Local<v8::Object>,
    name: &str,
) -> Option<v8::Local<'s, v8::Value>> {
    let key = v8::String::new(scope, name)?;
    obj.get(scope, key.into())
}

/// Define an own data property.
///
/// **Never `Set`.** Instance construction runs inside V8's
/// `DisallowJavascriptExecutionScope` (see `make_instance`), and an ordinary
/// `Set` that reaches a prototype-chain setter would execute JS and abort the
/// process. `CreateDataProperty` defines the property directly. workerd hit the
/// same wall and left the same note in `jsg/ser.c++`.
fn define(
    scope: &mut v8::PinScope,
    obj: v8::Local<v8::Object>,
    name: &str,
    value: v8::Local<v8::Value>,
) -> Option<()> {
    let key = v8::String::new(scope, name)?;
    obj.create_data_property(scope, key.into(), value)
        .map(|_| ())
}

/// Look up one of our classes from the private slot `install()` captured it
/// into.
///
/// Never `globalThis.<name>`: those properties are guest-writable, and this
/// lookup runs on the deserialize path where the result decides which
/// prototype a minted instance gets. The private read is also JS-free, which
/// `make_instance`'s `DisallowJavascriptExecutionScope` context requires.
fn class<'s>(scope: &mut v8::PinScope<'s, '_>, name: &str) -> Option<v8::Local<'s, v8::Function>> {
    let global = scope.get_current_context().global(scope);
    let key = class_key(scope, name)?;
    global.get_private(scope, key)?.try_into().ok()
}

/// Identify which of our types `obj` is by reading its private type stamp.
///
/// Only called for objects V8 has already routed to `write_host_object` — i.e.
/// objects carrying an internal field. Returns `None` for an internal-field
/// object that carries no stamp; no such object exists in this runtime today.
/// The stamp is written at construction and guest JS cannot reach it, so the
/// answer does not depend on `globalThis`, `Symbol.hasInstance`, or anything
/// else a run can mutate.
pub fn tag_of(scope: &mut v8::PinScope, obj: v8::Local<v8::Object>) -> Option<u32> {
    let key = tag_key(scope)?;
    let value = obj.get_private(scope, key)?;
    if !value.is_uint32() {
        return None;
    }
    value.uint32_value(scope)
}

pub fn headers_view<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    obj: v8::Local<v8::Object>,
) -> Option<HeadersView<'s>> {
    Some(HeadersView {
        list: get(scope, obj, "_l")?.try_into().ok()?,
    })
}

pub fn request_view<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    obj: v8::Local<v8::Object>,
) -> Option<RequestView<'s>> {
    Some(RequestView {
        url: get(scope, obj, "_u")?,
        method: get(scope, obj, "_m")?,
        headers: get(scope, obj, "_h")?.try_into().ok()?,
        body: get(scope, obj, "_b")?,
    })
}

pub fn response_view<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    obj: v8::Local<v8::Object>,
) -> Option<ResponseView<'s>> {
    let status = get(scope, obj, "_s")?.uint32_value(scope)?;
    Some(ResponseView {
        status,
        status_text: get(scope, obj, "_t")?,
        headers: get(scope, obj, "_h")?.try_into().ok()?,
        body: get(scope, obj, "_b")?,
    })
}

/// Mint an instance of one of our classes **without executing any JavaScript**.
///
/// This is the constraint that shapes the whole deserialize path: V8 runs
/// `ValueDeserializer::ReadHostObject` inside a
/// `DisallowJavascriptExecutionScope`, so calling the JS constructor here is
/// not slow — it is `V8_Fatal: Invoke in DisallowJavascriptExecutionScope` and
/// the process dies. workerd sidesteps it by constructing in C++
/// (`Response::constructor(js, …)` is a static C++ function, not a JS call).
/// We do the equivalent: build the object from an `ObjectTemplate` so it gets
/// the internal field V8 dispatches on, point its prototype at the JS class,
/// and define the state properties directly.
///
/// The codec has already validated the payload structurally, so skipping the
/// constructor also means a value round-trips exactly as sent rather than being
/// re-normalised (a GET with a body, an unusual status text).
fn make_instance<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    class_name: &str,
    tag: u32,
) -> Option<v8::Local<'s, v8::Object>> {
    // A fresh template per instance. Creating one is cheap next to the ~490 µs
    // isolate, and caching it would mean a per-isolate slot that has to be
    // invalidated on snapshot restore — more failure modes than it is worth
    // until a benchmark says otherwise.
    let tmpl = v8::ObjectTemplate::new(scope);
    tmpl.set_internal_field_count(INTERNAL_FIELD_COUNT);
    let obj = tmpl.new_instance(scope)?;

    // The internal field stays empty — see the module docs. The type identity
    // is the private stamp: a minted instance must re-serialize under the same
    // tag it arrived with, exactly like a constructed one.
    stamp_tag(scope, obj, tag)?;

    // `instanceof` and every prototype method come from here — the reference
    // captured at install(), untouched by whatever the guest did to the
    // `globalThis` property of the same name.
    let ctor = class(scope, class_name)?;
    let key = v8::String::new(scope, "prototype")?;
    let proto = ctor.get(scope, key.into())?;
    obj.set_prototype(scope, proto)?;

    Some(obj)
}

/// Build a `Headers` from a flat `[name, value, …]` array.
pub fn make_headers<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    list: v8::Local<v8::Array>,
) -> Option<v8::Local<'s, v8::Object>> {
    let obj = make_instance(scope, "Headers", TAG_HEADERS)?;
    define(scope, obj, "_l", list.into())?;
    Some(obj)
}

pub fn make_request<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    url: v8::Local<v8::Value>,
    method: v8::Local<v8::Value>,
    headers: v8::Local<v8::Object>,
    body: v8::Local<v8::Value>,
) -> Option<v8::Local<'s, v8::Object>> {
    let obj = make_instance(scope, "Request", TAG_REQUEST)?;
    define(scope, obj, "_u", url)?;
    define(scope, obj, "_m", method)?;
    define(scope, obj, "_h", headers.into())?;
    define(scope, obj, "_b", body)?;
    let used = v8::Boolean::new(scope, false);
    define(scope, obj, "_used", used.into())?;
    Some(obj)
}

pub fn make_response<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    status: u32,
    status_text: v8::Local<v8::Value>,
    headers: v8::Local<v8::Object>,
    body: v8::Local<v8::Value>,
) -> Option<v8::Local<'s, v8::Object>> {
    let obj = make_instance(scope, "Response", TAG_RESPONSE)?;
    let status_value = v8::Integer::new_from_unsigned(scope, status);
    define(scope, obj, "_s", status_value.into())?;
    define(scope, obj, "_t", status_text)?;
    define(scope, obj, "_h", headers.into())?;
    define(scope, obj, "_b", body)?;
    let used = v8::Boolean::new(scope, false);
    define(scope, obj, "_used", used.into())?;
    Some(obj)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run `body` in a fresh isolate + context with the web globals installed.
    fn with_web<R>(body: impl FnOnce(&mut v8::PinScope) -> R) -> R {
        crate::v8::init_platform();
        let isolate = &mut v8::Isolate::new(Default::default());
        v8::scope!(let scope, isolate);
        let context = v8::Context::new(scope, Default::default());
        let scope = &mut v8::ContextScope::new(scope, context);
        install(scope).expect("install web globals");
        body(scope)
    }

    fn eval_str(scope: &mut v8::PinScope, src: &str) -> String {
        let s = v8::String::new(scope, src).unwrap();
        v8::tc_scope!(let tc, scope);
        let script = match v8::Script::compile(tc, s, None) {
            Some(sc) => sc,
            None => return exception_text(tc, "compile"),
        };
        match script.run(tc) {
            Some(v) => v.to_string(tc).unwrap().to_rust_string_lossy(tc),
            None => exception_text(tc, "run"),
        }
    }

    #[test]
    fn classes_are_installed_and_shells_are_not_exposed() {
        let out = with_web(|scope| {
            eval_str(
                scope,
                "[typeof Headers, typeof Request, typeof Response, typeof TextEncoder, \
                  typeof TextDecoder, typeof URL, typeof URLSearchParams, \
                  typeof globalThis.HeadersShell].join(',')",
            )
        });
        assert_eq!(
            out,
            "function,function,function,function,function,function,function,undefined"
        );
    }

    #[test]
    fn instances_carry_internal_fields() {
        let out = with_web(|scope| {
            let s = v8::String::new(scope, "new Response('hi')").unwrap();
            let script = v8::Script::compile(scope, s, None).unwrap();
            let value = script.run(scope).unwrap();
            let obj: v8::Local<v8::Object> = value.try_into().unwrap();
            obj.internal_field_count()
        });
        assert_eq!(out, 1, "Response instances must carry the routing field");
    }

    /// Compile and run `src`, returning the produced value as an object.
    fn eval_obj<'s>(scope: &mut v8::PinScope<'s, '_>, src: &str) -> v8::Local<'s, v8::Object> {
        let s = v8::String::new(scope, src).unwrap();
        let script = v8::Script::compile(scope, s, None).unwrap();
        script.run(scope).unwrap().try_into().unwrap()
    }

    #[test]
    fn tagging_does_not_consult_globalthis() {
        // Guest code can delete or shadow every one of our globals. Identity
        // is the construction-time stamp, so intact instances still tag.
        let out = with_web(|scope| {
            let pair = eval_obj(
                scope,
                "(() => { const h = new Headers([['x-a','1']]); \
                   const r = new Response('x'); \
                   delete globalThis.Headers; delete globalThis.Request; \
                   delete globalThis.Response; return [h, r] })()",
            );
            let array: v8::Local<v8::Array> = pair.try_into().unwrap();
            let headers: v8::Local<v8::Object> =
                array.get_index(scope, 0).unwrap().try_into().unwrap();
            let response: v8::Local<v8::Object> =
                array.get_index(scope, 1).unwrap().try_into().unwrap();
            (tag_of(scope, headers), tag_of(scope, response))
        });
        assert_eq!(out, (Some(TAG_HEADERS), Some(TAG_RESPONSE)));
    }

    #[test]
    fn an_overridden_hasinstance_cannot_mistag() {
        // `instanceof` consults Symbol.hasInstance, which guest code can
        // override to claim everything. The stamp does not.
        let out = with_web(|scope| {
            let obj = eval_obj(
                scope,
                "Object.defineProperty(Response, Symbol.hasInstance, { value: () => true }); \
                 Object.defineProperty(Request, Symbol.hasInstance, { value: () => true }); \
                 new Headers([['x-a','1']])",
            );
            tag_of(scope, obj)
        });
        assert_eq!(out, Some(TAG_HEADERS));
    }

    #[test]
    fn a_prototype_lookalike_carries_no_tag() {
        // Re-pointing a prototype does not run a constructor, so there is no
        // stamp (and no internal field) — the object is not one of ours.
        let out = with_web(|scope| {
            let obj = eval_obj(
                scope,
                "Object.setPrototypeOf({ _l: ['x-a','1'] }, Headers.prototype)",
            );
            tag_of(scope, obj)
        });
        assert_eq!(out, None);
    }

    #[test]
    fn the_stamp_is_unreachable_from_guest_js() {
        // Private symbols do not surface through any reflection API, so guest
        // code cannot read, copy, or delete the tag.
        let out = with_web(|scope| {
            eval_str(
                scope,
                "const r = new Response('x'); \
                 [Object.getOwnPropertySymbols(r).length, \
                  Reflect.ownKeys(r).some(k => typeof k === 'symbol')].join('|')",
            )
        });
        assert_eq!(out, "0|false");
    }

    #[test]
    fn headers_are_case_insensitive_and_keep_duplicates() {
        let out = with_web(|scope| {
            eval_str(
                scope,
                "const h = new Headers([['Content-Type','text/plain']]); \
                 h.append('Set-Cookie','a=1'); h.append('set-cookie','b=2'); \
                 [h.get('CONTENT-TYPE'), h.get('set-cookie'), \
                  JSON.stringify(h.getSetCookie()), h.has('nope')].join('|')",
            )
        });
        assert_eq!(out, "text/plain|a=1, b=2|[\"a=1\",\"b=2\"]|false");
    }

    #[test]
    fn response_body_helpers_round_trip() {
        let out = with_web(|scope| {
            eval_str(
                scope,
                "const r = new Response(JSON.stringify({a:1}), \
                   { status: 201, headers: { 'content-type': 'application/json' } }); \
                 Promise.resolve().then(() => 0); \
                 [r.status, r.ok, r.headers.get('content-type')].join('|')",
            )
        });
        assert_eq!(out, "201|true|application/json");
    }

    #[test]
    fn request_normalises_method_and_url() {
        let out = with_web(|scope| {
            eval_str(
                scope,
                "const r = new Request('https://ex.com/a/../b?x=1', { method: 'post' }); \
                 [r.method, r.url].join('|')",
            )
        });
        assert_eq!(out, "POST|https://ex.com/b?x=1");
    }

    #[test]
    fn text_codecs_round_trip_utf8() {
        let out = with_web(|scope| {
            eval_str(
                scope,
                "const e = new TextEncoder(); const d = new TextDecoder(); \
                 const bytes = e.encode('héllo — 世界'); \
                 [bytes.length, d.decode(bytes)].join('|')",
            )
        });
        // 1 + 2 + 1 + 1 + 1 + 1 + 3 + 1 + 3 + 3
        assert_eq!(out, "17|héllo — 世界");
    }

    #[test]
    fn url_parses_and_serialises() {
        let out = with_web(|scope| {
            eval_str(
                scope,
                "const u = new URL('/p/q?a=1&b=2#f', 'https://user@ex.com:8443'); \
                 [u.protocol, u.host, u.hostname, u.port, u.pathname, u.search, u.hash, \
                  u.searchParams.get('b'), u.href].join('|')",
            )
        });
        assert_eq!(
            out,
            "https:|ex.com:8443|ex.com|8443|/p/q|?a=1&b=2|#f|2|https://user@ex.com:8443/p/q?a=1&b=2#f"
        );
    }
}
