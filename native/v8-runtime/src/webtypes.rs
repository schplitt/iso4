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
//! The shell also stores its type tag in internal field 0, so
//! `write_host_object` dispatches in O(1) with no prototype walk.
//!
//! Everything above the shell — the actual spec behaviour — is JS, evaluated
//! once into the context. A JS subclass of a shell keeps the internal field,
//! which is what makes this split possible.
//!
//! # Snapshot coupling — read this before touching either call site
//!
//! Native callbacks cannot be serialized into a V8 startup snapshot unless the
//! embedder supplies an `ExternalReferences` table, and the **same** table must
//! be supplied again when the snapshot is restored. The two call sites are
//! `Isolate::snapshot_creator(...)` in `precompile_module` and
//! `CreateParams::external_references(...)` in `run_module`. They move
//! together. Supplying it at snapshot time but not at restore time does not
//! fail cleanly: `typeof Response` still reports `"function"`, and the process
//! then dies with `V8_Fatal: No external references provided via API` on the
//! first `new Response()`.

use std::sync::OnceLock;

use v8::MapFnTo;

use crate::webcodec::{TAG_HEADERS, TAG_REQUEST, TAG_RESPONSE};

/// Internal-field index holding the type tag. Index 0 of 1.
pub const TAG_FIELD: usize = 0;

// ── Native shells ────────────────────────────────────────────────────────────

/// Stamp the type tag into the instance's internal field.
///
/// Invoked through `super()` from the JS subclass, so `args.this()` is the
/// instance V8 built from our instance template.
fn stamp(scope: &mut v8::HandleScope, args: &v8::FunctionCallbackArguments, tag: u32) {
    let this = args.this();
    // A shell is only ever reachable via `super()` from its own JS subclass,
    // but a hostile prototype chain could in principle route elsewhere. Setting
    // the field on an object that has none would abort, so check first.
    if this.internal_field_count() > TAG_FIELD {
        let value = v8::Integer::new_from_unsigned(scope, tag);
        this.set_internal_field(TAG_FIELD, value.into());
    }
}

fn headers_shell_ctor(
    scope: &mut v8::HandleScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    stamp(scope, &args, TAG_HEADERS);
}

fn request_shell_ctor(
    scope: &mut v8::HandleScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    stamp(scope, &args, TAG_REQUEST);
}

fn response_shell_ctor(
    scope: &mut v8::HandleScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    stamp(scope, &args, TAG_RESPONSE);
}

static EXTERNAL_REFS: OnceLock<v8::ExternalReferences> = OnceLock::new();

/// The external-reference table for every native callback that can end up in a
/// snapshot. Must be passed to **both** `Isolate::snapshot_creator` and
/// `CreateParams::external_references` — see the module docs.
pub fn external_references() -> &'static v8::ExternalReferences {
    EXTERNAL_REFS.get_or_init(|| {
        v8::ExternalReferences::new(&[
            v8::ExternalReference {
                function: headers_shell_ctor.map_fn_to(),
            },
            v8::ExternalReference {
                function: request_shell_ctor.map_fn_to(),
            },
            v8::ExternalReference {
                function: response_shell_ctor.map_fn_to(),
            },
        ])
    })
}

// ── Installation ─────────────────────────────────────────────────────────────

/// Install the web globals into the current context.
///
/// Called at context creation on every path — both precompile passes and the
/// no-prefix run path — so prefix and postfix code see the same environment.
/// When a prefix snapshot is restored the classes come back with it and this is
/// not called again.
pub fn install(scope: &mut v8::HandleScope) -> Result<(), String> {
    let mut shells = Vec::with_capacity(3);
    for (name, ctor) in [
        ("HeadersShell", headers_shell_ctor.map_fn_to()),
        ("RequestShell", request_shell_ctor.map_fn_to()),
        ("ResponseShell", response_shell_ctor.map_fn_to()),
    ] {
        let tmpl = v8::FunctionTemplate::builder_raw(ctor).build(scope);
        tmpl.instance_template(scope).set_internal_field_count(1);
        let class_name =
            v8::String::new(scope, name).ok_or_else(|| format!("intern {name} failed"))?;
        tmpl.set_class_name(class_name);
        let func = tmpl
            .get_function(scope)
            .ok_or_else(|| format!("get_function for {name} failed"))?;
        shells.push(v8::Local::<v8::Value>::from(func));
    }

    // The runtime source is an expression evaluating to a function; calling it
    // with the three shells installs the classes. The shells themselves are
    // never exposed on `globalThis`.
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

    let tc = &mut v8::TryCatch::new(scope);
    let script = v8::Script::compile(tc, source, Some(&origin))
        .ok_or_else(|| exception_text(tc, "compile web runtime"))?;
    let factory = script
        .run(tc)
        .ok_or_else(|| exception_text(tc, "evaluate web runtime"))?;
    let factory: v8::Local<v8::Function> = factory
        .try_into()
        .map_err(|_| "web runtime did not evaluate to a function".to_string())?;

    let recv = v8::undefined(tc).into();
    factory
        .call(tc, recv, &shells)
        .ok_or_else(|| exception_text(tc, "install web runtime"))?;
    Ok(())
}

fn exception_text(tc: &mut v8::TryCatch<v8::HandleScope>, what: &str) -> String {
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
    scope: &mut v8::HandleScope<'s>,
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
    scope: &mut v8::HandleScope,
    obj: v8::Local<v8::Object>,
    name: &str,
    value: v8::Local<v8::Value>,
) -> Option<()> {
    let key = v8::String::new(scope, name)?;
    obj.create_data_property(scope, key.into(), value).map(|_| ())
}

/// Look up one of our classes on `globalThis`.
///
/// Resolved per call rather than cached in a `Global`: a prefix snapshot is
/// restored into a fresh isolate per run, so any cache would have to be
/// per-run anyway, and this is a single property get on a value we are about
/// to do far more work with.
fn class<'s>(scope: &mut v8::HandleScope<'s>, name: &str) -> Option<v8::Local<'s, v8::Function>> {
    let global = scope.get_current_context().global(scope);
    let key = v8::String::new(scope, name)?;
    global.get(scope, key.into())?.try_into().ok()
}

/// Read the type tag a shell constructor stamped into internal field 0.
/// `None` for objects that are not ours.
pub fn tag_of(scope: &mut v8::HandleScope, obj: v8::Local<v8::Object>) -> Option<u32> {
    if obj.internal_field_count() <= TAG_FIELD {
        return None;
    }
    let field = obj.get_internal_field(scope, TAG_FIELD)?;
    let value: v8::Local<v8::Value> = field.try_into().ok()?;
    if !value.is_uint32() {
        return None;
    }
    Some(value.uint32_value(scope)?)
}

pub fn headers_view<'s>(
    scope: &mut v8::HandleScope<'s>,
    obj: v8::Local<v8::Object>,
) -> Option<HeadersView<'s>> {
    Some(HeadersView {
        list: get(scope, obj, "_l")?.try_into().ok()?,
    })
}

pub fn request_view<'s>(
    scope: &mut v8::HandleScope<'s>,
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
    scope: &mut v8::HandleScope<'s>,
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
    scope: &mut v8::HandleScope<'s>,
    class_name: &str,
    tag: u32,
) -> Option<v8::Local<'s, v8::Object>> {
    // A fresh template per instance. Creating one is cheap next to the ~490 µs
    // isolate, and caching it would mean a per-isolate slot that has to be
    // invalidated on snapshot restore — more failure modes than it is worth
    // until a benchmark says otherwise.
    let tmpl = v8::ObjectTemplate::new(scope);
    tmpl.set_internal_field_count(1);
    let obj = tmpl.new_instance(scope)?;

    let tag_value = v8::Integer::new_from_unsigned(scope, tag);
    obj.set_internal_field(TAG_FIELD, tag_value.into());

    // `instanceof` and every prototype method come from here.
    let ctor = class(scope, class_name)?;
    let key = v8::String::new(scope, "prototype")?;
    let proto = ctor.get(scope, key.into())?;
    obj.set_prototype(scope, proto)?;

    Some(obj)
}

/// Build a `Headers` from a flat `[name, value, …]` array.
pub fn make_headers<'s>(
    scope: &mut v8::HandleScope<'s>,
    list: v8::Local<v8::Array>,
) -> Option<v8::Local<'s, v8::Object>> {
    let obj = make_instance(scope, "Headers", TAG_HEADERS)?;
    define(scope, obj, "_l", list.into())?;
    Some(obj)
}

pub fn make_request<'s>(
    scope: &mut v8::HandleScope<'s>,
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
    scope: &mut v8::HandleScope<'s>,
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

/// Does `obj` have one of our class prototypes in its chain without carrying an
/// internal field?
///
/// This catches *lookalikes* — classes that re-point their prototype after the
/// fact instead of calling our constructor, which is how srvx's `FastResponse`
/// is built. They serialize only at the top level of a slot; see
/// `docs/protocol.md` §4.4.6.
pub fn lookalike_tag(scope: &mut v8::HandleScope, obj: v8::Local<v8::Object>) -> Option<u32> {
    if obj.internal_field_count() > TAG_FIELD {
        return None;
    }
    for (name, tag) in [
        ("Response", TAG_RESPONSE),
        ("Request", TAG_REQUEST),
        ("Headers", TAG_HEADERS),
    ] {
        if let Some(ctor) = class(scope, name) {
            if obj.instance_of(scope, ctor.into()) == Some(true) {
                return Some(tag);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run `body` in a fresh isolate + context with the web globals installed.
    fn with_web<R>(body: impl FnOnce(&mut v8::HandleScope) -> R) -> R {
        crate::v8::init_platform();
        let isolate = &mut v8::Isolate::new(Default::default());
        let scope = &mut v8::HandleScope::new(isolate);
        let context = v8::Context::new(scope, Default::default());
        let scope = &mut v8::ContextScope::new(scope, context);
        install(scope).expect("install web globals");
        body(scope)
    }

    fn eval_str(scope: &mut v8::HandleScope, src: &str) -> String {
        let s = v8::String::new(scope, src).unwrap();
        let tc = &mut v8::TryCatch::new(scope);
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
