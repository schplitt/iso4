//! Host-type codec — the payload that follows V8's `kHostObject` tag.
//!
//! Wire format: `docs/protocol.md` §4.4. This module owns the bytes. It reads
//! and writes sandbox instance state exclusively through `webtypes`' adapter,
//! so swapping the sandbox runtime does not touch this file.
//!
//! # Adding a type
//!
//! 1. Allocate a tag below and record it in `docs/protocol.md` §4.4.1. Tags are
//!    frozen once anything has written them.
//! 2. Add an `encode_*` / `decode_*` pair.
//! 3. Add the two match arms in `encode` / `decode`.
//!
//! Nothing else in the protocol changes. A type that cannot be represented in
//! this build reports `CodecError::Unsupported`, which the caller maps to
//! `ERR_TYPE_NOT_SERIALIZABLE`.
//!
//! # The two directions use different mechanisms
//!
//! **Sandbox → host** writes real V8 host objects: the instances carry internal
//! fields, so V8 routes them to `write_host_object` itself, at any depth, with
//! no cost for ordinary values.
//!
//! **Host → sandbox** cannot do that. Node's `v8.Serializer` exposes no
//! delegate to JavaScript, so the host cannot emit a host object at all. Instead
//! it substitutes each instance with a **branded plain object** carrying the
//! same fields, serializes the graph normally, and `rehydrate` (below) walks the
//! result swapping brands for real instances. That works at any depth, for any
//! type, and needs no hand-written framing.
//!
//! The asymmetry is safe because the two directions never share a reader.

use std::cell::RefCell;

use v8::{ValueDeserializerHelper, ValueSerializerHelper};

use crate::webtypes;

// ── Type tags (frozen — see docs/protocol.md §4.4.1) ─────────────────────────

pub const TAG_INVALID: u32 = 0;
pub const TAG_HEADERS: u32 = 1;
pub const TAG_REQUEST: u32 = 2;
pub const TAG_RESPONSE: u32 = 3;
pub const TAG_READABLE_STREAM: u32 = 4;
pub const TAG_WRITABLE_STREAM: u32 = 5;
pub const TAG_WEB_SOCKET: u32 = 6;
pub const TAG_ABORT_SIGNAL: u32 = 7;

/// Names for error messages. Reserved-but-unimplemented tags are listed so a
/// peer built with a wider type set produces a message that says what it sent.
fn tag_name(tag: u32) -> &'static str {
    match tag {
        TAG_HEADERS => "Headers",
        TAG_REQUEST => "Request",
        TAG_RESPONSE => "Response",
        TAG_READABLE_STREAM => "ReadableStream",
        TAG_WRITABLE_STREAM => "WritableStream",
        TAG_WEB_SOCKET => "WebSocket",
        TAG_ABORT_SIGNAL => "AbortSignal",
        _ => "unknown type",
    }
}

/// Guard against a hostile or corrupt peer claiming an enormous header count
/// before we allocate. Mirrors workerd's identical limit.
const MAX_HEADER_ENTRIES: u32 = 1024;

// ── Errors ───────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum CodecError {
    /// The value cannot cross this boundary at all — an unimplemented tag, or
    /// content that is not self-contained. Maps to `ERR_TYPE_NOT_SERIALIZABLE`.
    Unsupported(String),
    /// Malformed bytes, or a V8 operation that failed. Maps to `ERR_INTERNAL`.
    Malformed(String),
}

impl CodecError {
    pub fn message(&self) -> &str {
        match self {
            CodecError::Unsupported(m) | CodecError::Malformed(m) => m,
        }
    }
}

fn malformed<T>(what: &str) -> Result<T, CodecError> {
    Err(CodecError::Malformed(format!(
        "host-type payload is malformed: {what}"
    )))
}

type Codec<T> = Result<T, CodecError>;

// ── Primitives ───────────────────────────────────────────────────────────────

fn write_bytes(helper: &dyn ValueSerializerHelper, bytes: &[u8]) {
    helper.write_uint32(bytes.len() as u32);
    helper.write_raw_bytes(bytes);
}

fn read_bytes(helper: &dyn ValueDeserializerHelper) -> Codec<&[u8]> {
    let mut len = 0u32;
    if !helper.read_uint32(&mut len) {
        return malformed("truncated length");
    }
    match helper.read_raw_bytes(len as usize) {
        Some(b) => Ok(b),
        None => malformed("truncated bytes"),
    }
}

fn read_string(helper: &dyn ValueDeserializerHelper) -> Codec<String> {
    let bytes = read_bytes(helper)?;
    String::from_utf8(bytes.to_vec()).or_else(|_| malformed("string is not valid UTF-8"))
}

fn read_u32(helper: &dyn ValueDeserializerHelper) -> Codec<u32> {
    let mut v = 0u32;
    if !helper.read_uint32(&mut v) {
        return malformed("truncated integer");
    }
    Ok(v)
}

/// A V8 string written as UTF-8 bytes. Goes through `to_rust_string_lossy`
/// rather than `write_value` so the reader can rebuild it without a nested
/// deserialize — the host side has to be able to hand-write this.
fn write_v8_string(
    scope: &mut v8::PinScope,
    helper: &dyn ValueSerializerHelper,
    value: v8::Local<v8::Value>,
) -> Codec<()> {
    let s = match value.to_string(scope) {
        Some(s) => s.to_rust_string_lossy(scope),
        None => return malformed("value is not stringifiable"),
    };
    write_bytes(helper, s.as_bytes());
    Ok(())
}

fn v8_string<'s>(scope: &mut v8::PinScope<'s, '_>, s: &str) -> Codec<v8::Local<'s, v8::Value>> {
    match v8::String::new(scope, s) {
        Some(v) => Ok(v.into()),
        None => malformed("could not intern string"),
    }
}

// ── Headers ──────────────────────────────────────────────────────────────────

fn encode_headers(
    scope: &mut v8::PinScope,
    helper: &dyn ValueSerializerHelper,
    headers: v8::Local<v8::Object>,
) -> Codec<()> {
    let view = match webtypes::headers_view(scope, headers) {
        Some(v) => v,
        None => return malformed("Headers instance has no entry list"),
    };
    // The list is flat [name, value, …], so entry count is half its length.
    let flat = view.list.length();
    if !flat.is_multiple_of(2) {
        return malformed("Headers entry list has an odd length");
    }
    if flat / 2 > MAX_HEADER_ENTRIES {
        return Err(CodecError::Unsupported(format!(
            "Headers has {} entries, exceeding the {MAX_HEADER_ENTRIES} limit",
            flat / 2
        )));
    }
    // Hand the existing array to V8 rather than writing 2N strings across the
    // API boundary. See the module docs on why per-element work is avoided.
    let context = scope.get_current_context();
    match helper.write_value(context, view.list.into()) {
        Some(true) => Ok(()),
        _ => malformed("could not write the Headers entry list"),
    }
}

fn decode_headers<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    helper: &dyn ValueDeserializerHelper,
) -> Codec<v8::Local<'s, v8::Object>> {
    let context = scope.get_current_context();
    let value = match helper.read_value(context) {
        Some(v) => v,
        None => return malformed("truncated Headers entry list"),
    };
    let list: v8::Local<v8::Array> = match value.try_into() {
        Ok(a) => a,
        Err(_) => return malformed("Headers entry list is not an array"),
    };
    let flat = list.length();
    if !flat.is_multiple_of(2) {
        return malformed("Headers entry list has an odd length");
    }
    if flat / 2 > MAX_HEADER_ENTRIES {
        return Err(CodecError::Unsupported(format!(
            "Headers payload declares {} entries, exceeding the {MAX_HEADER_ENTRIES} limit",
            flat / 2
        )));
    }

    match webtypes::make_headers(scope, list) {
        Some(h) => Ok(h),
        None => malformed("could not construct Headers"),
    }
}

// ── Bodies ───────────────────────────────────────────────────────────────────

/// The protocol permits exactly `null | string | Uint8Array`.
///
/// Checked in one place, used by the writer and by descriptor rehydration.
/// Accepting any `ArrayBufferView` here would let a `DataView` or `Uint16Array`
/// serialize out of Rust and then be refused by the host reader — turning a
/// user value into a host-side decode failure.
fn check_body(body: v8::Local<v8::Value>) -> Codec<v8::Local<v8::Value>> {
    let ok = body.is_null_or_undefined() || body.is_string() || body.is_uint8_array();
    if ok {
        Ok(body)
    } else {
        Err(CodecError::Unsupported(
            "body must be null, a string, or a Uint8Array — a stream cannot cross the \
             sandbox boundary; buffer it first (await res.arrayBuffer())"
                .to_string(),
        ))
    }
}

fn encode_body(
    scope: &mut v8::PinScope,
    helper: &dyn ValueSerializerHelper,
    body: v8::Local<v8::Value>,
) -> Codec<()> {
    if crate::v8::body_stream_id_of(scope, body).is_some() {
        return Err(CodecError::Unsupported(
            "a streamed body cannot be returned to the host yet — read it first \
             (await res.arrayBuffer()) and return the bytes"
                .to_string(),
        ));
    }
    let body = check_body(body)?;
    // V8 writes the bytes once, straight out of the backing store. Framing it
    // by hand meant copy_contents into a Vec and then a second copy into the
    // serializer.
    let context = scope.get_current_context();
    match helper.write_value(context, body) {
        Some(true) => Ok(()),
        _ => malformed("could not write the body"),
    }
}

fn decode_body<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    helper: &dyn ValueDeserializerHelper,
) -> Codec<v8::Local<'s, v8::Value>> {
    let context = scope.get_current_context();
    // null, a string, or a Uint8Array — V8 preserves which, so there is no
    // kind byte to agree on. Still validated: a spoofed payload must not be
    // able to hand user code a Response whose body is an arbitrary object.
    match helper.read_value(context) {
        Some(v) => check_body(v),
        None => malformed("truncated body"),
    }
}

// ── Extras ───────────────────────────────────────────────────────────────────
//
// A length-delimited V8 blob holding forward-compatible fields. Written as a
// self-contained sub-blob rather than a nested `write_value` so the host side,
// which hand-writes these payloads, can produce the identical bytes.

fn write_no_extras(helper: &dyn ValueSerializerHelper) {
    helper.write_uint32(0);
}

fn skip_extras(helper: &dyn ValueDeserializerHelper) -> Codec<()> {
    read_bytes(helper).map(|_| ())
}

// ── Request / Response ───────────────────────────────────────────────────────

fn encode_request(
    scope: &mut v8::PinScope,
    helper: &dyn ValueSerializerHelper,
    obj: v8::Local<v8::Object>,
) -> Codec<()> {
    let view = match webtypes::request_view(scope, obj) {
        Some(v) => v,
        None => return malformed("Request instance is missing internal state"),
    };
    write_v8_string(scope, helper, view.url)?;
    write_v8_string(scope, helper, view.method)?;
    encode_headers(scope, helper, view.headers)?;
    encode_body(scope, helper, view.body)?;
    write_no_extras(helper);
    Ok(())
}

fn decode_request<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    helper: &dyn ValueDeserializerHelper,
) -> Codec<v8::Local<'s, v8::Object>> {
    let url = read_string(helper)?;
    let method = read_string(helper)?;
    let headers = decode_headers(scope, helper)?;
    let body = decode_body(scope, helper)?;
    skip_extras(helper)?;

    let url_v = v8_string(scope, &url)?;
    let method_v = v8_string(scope, &method)?;
    match webtypes::make_request(scope, url_v, method_v, headers, body) {
        Some(r) => Ok(r),
        None => malformed("could not construct Request"),
    }
}

fn encode_response(
    scope: &mut v8::PinScope,
    helper: &dyn ValueSerializerHelper,
    obj: v8::Local<v8::Object>,
) -> Codec<()> {
    let view = match webtypes::response_view(scope, obj) {
        Some(v) => v,
        None => return malformed("Response instance is missing internal state"),
    };
    helper.write_uint32(view.status);
    write_v8_string(scope, helper, view.status_text)?;
    encode_headers(scope, helper, view.headers)?;
    encode_body(scope, helper, view.body)?;
    write_no_extras(helper);
    Ok(())
}

fn decode_response<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    helper: &dyn ValueDeserializerHelper,
) -> Codec<v8::Local<'s, v8::Object>> {
    let status = read_u32(helper)?;
    let status_text = read_string(helper)?;
    let headers = decode_headers(scope, helper)?;
    let body = decode_body(scope, helper)?;
    skip_extras(helper)?;

    let status_text_v = v8_string(scope, &status_text)?;
    match webtypes::make_response(scope, status, status_text_v, headers, body) {
        Some(r) => Ok(r),
        None => malformed("could not construct Response"),
    }
}

// ── Registry ─────────────────────────────────────────────────────────────────

/// Write `obj` as a host-object payload: the type tag, then the type's body.
///
/// The caller has already established that this is one of ours — V8 routed it
/// here via an internal field, and `tag` is the private type stamp
/// `webtypes::tag_of` read off the instance.
pub fn encode(
    scope: &mut v8::PinScope,
    helper: &dyn ValueSerializerHelper,
    obj: v8::Local<v8::Object>,
    tag: u32,
) -> Codec<()> {
    helper.write_uint32(tag);
    match tag {
        TAG_HEADERS => encode_headers(scope, helper, obj),
        TAG_REQUEST => encode_request(scope, helper, obj),
        TAG_RESPONSE => encode_response(scope, helper, obj),
        TAG_INVALID => malformed("refusing to write the reserved invalid tag"),
        other => Err(CodecError::Unsupported(format!(
            "{} cannot be serialized by this build",
            tag_name(other)
        ))),
    }
}

/// Read a host-object payload, tag first.
pub fn decode<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    helper: &dyn ValueDeserializerHelper,
) -> Codec<v8::Local<'s, v8::Object>> {
    let tag = read_u32(helper)?;
    match tag {
        TAG_HEADERS => decode_headers(scope, helper),
        TAG_REQUEST => decode_request(scope, helper),
        TAG_RESPONSE => decode_response(scope, helper),
        TAG_INVALID => malformed("payload carries the reserved invalid tag"),
        other => Err(CodecError::Unsupported(format!(
            "received a {} (tag {other}); this build cannot materialise one",
            tag_name(other)
        ))),
    }
}

// ── Rehydration (host → sandbox) ─────────────────────────────────────────────

/// Prefix of the property name marking a branded descriptor. The full brand
/// key is this prefix followed by the session's descriptor token in lowercase
/// hex, so only descriptors the host actually stamped rehydrate: an inbound
/// object carrying the bare prefix — or any key with a wrong token — is
/// ordinary data and passes through untouched. Wire contract — kept in sync
/// with `BRAND_PREFIX` in `packages/iso4-sandbox/src/web-codec.ts`.
pub const BRAND_PREFIX: &str = "__iso4_ht_";

/// Exact byte length of the descriptor token carried in the `Authenticate`
/// frame (`docs/protocol.md` §5.1). 128 random bits: unguessable, and never
/// surfaced to guest JS (descriptors are replaced wholesale before a value
/// reaches the isolate's code).
pub const DESCRIPTOR_TOKEN_LEN: usize = 16;

/// Build the session brand key from a descriptor token.
pub fn brand_key_for_token(token: &[u8]) -> String {
    use std::fmt::Write;
    let mut key = String::with_capacity(BRAND_PREFIX.len() + token.len() * 2);
    key.push_str(BRAND_PREFIX);
    for byte in token {
        let _ = write!(key, "{byte:02x}");
    }
    key
}

thread_local! {
    /// The brand key for descriptors deserialized on this thread. Set from the
    /// connection's handshake token on the session thread, and on each warm
    /// instance owner thread at spawn. Unset (e.g. unit tests driving the run
    /// paths directly) is fail-closed: nothing rehydrates and branded-looking
    /// objects pass through as the plain data they are.
    static SESSION_BRAND_KEY: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Install the brand key for host-type descriptors handled on this thread.
pub fn set_session_brand_key(key: String) {
    SESSION_BRAND_KEY.with(|slot| *slot.borrow_mut() = Some(key));
}

/// The brand key installed on this thread, if any.
pub fn session_brand_key() -> Option<String> {
    SESSION_BRAND_KEY.with(|slot| slot.borrow().clone())
}

/// Cheap pre-check: could this blob contain a stamped descriptor at all?
///
/// V8 writes property names as literal bytes, so if the brand key is present
/// it appears verbatim. A false positive costs one harmless walk; a false
/// negative is impossible. The needle contains the session's random token, so
/// data that merely resembles a descriptor never gets past this scan — the
/// overwhelmingly common path pays the scan and nothing else.
///
/// Two-phase rather than `windows().any()`: scan for the needle's first byte
/// (a tight byte-equality loop LLVM vectorises), and run the full comparison
/// only at candidate positions — for payload-sized blobs the per-window
/// `bcmp` of the naive form was the measurable cost, ~1.5 ms on a 2 MB
/// data global.
pub fn might_contain_web_types(blob: &[u8], brand_key: &str) -> bool {
    let needle = brand_key.as_bytes();
    let Some((&first, rest)) = needle.split_first() else {
        return false;
    };
    if blob.len() < needle.len() {
        return false;
    }
    // Candidates can only start where the whole needle still fits.
    let last_start = blob.len() - needle.len();
    let mut from = 0;
    while let Some(offset) = blob[from..].iter().position(|&b| b == first) {
        let start = from + offset;
        if start > last_start {
            return false;
        }
        if &blob[start + 1..start + needle.len()] == rest {
            return true;
        }
        from = start + 1;
    }
    false
}

/// Replace every stamped descriptor in `value` with a real instance, in place.
///
/// Depth-limited rather than cycle-tracked: V8 preserves object identity, so a
/// cyclic graph would otherwise recurse forever. 32 levels is far past anything
/// a request/response payload needs, and exceeding it is a refusal rather than
/// silent truncation.
pub fn rehydrate<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    value: v8::Local<'s, v8::Value>,
    brand_key: &str,
) -> Codec<v8::Local<'s, v8::Value>> {
    // Interned once for the whole walk; V8 caches the string's hash after the
    // first lookup, so per-object cost does not depend on the key's length.
    let Some(key) = v8::String::new(scope, brand_key) else {
        return malformed("could not intern the brand key");
    };
    rehydrate_at(scope, value, key, 0)
}

const MAX_DEPTH: u32 = 32;

fn rehydrate_at<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    value: v8::Local<'s, v8::Value>,
    brand_key: v8::Local<'s, v8::String>,
    depth: u32,
) -> Codec<v8::Local<'s, v8::Value>> {
    if depth > MAX_DEPTH {
        return Err(CodecError::Unsupported(format!(
            "value nests deeper than {MAX_DEPTH} levels; cannot scan it for host types"
        )));
    }
    if !value.is_object() || value.is_array_buffer_view() || value.is_array_buffer() {
        return Ok(value);
    }
    let obj: v8::Local<v8::Object> = match value.try_into() {
        Ok(o) => o,
        Err(_) => return Ok(value),
    };

    // A stamped descriptor is replaced wholesale; its own fields are plain data.
    if let Some(tag) = brand_of(scope, obj, brand_key) {
        return build_from_descriptor(scope, obj, tag).map(|o| o.into());
    }

    if let Ok(array) = v8::Local::<v8::Array>::try_from(value) {
        for i in 0..array.length() {
            if let Some(item) = array.get_index(scope, i) {
                let replaced = rehydrate_at(scope, item, brand_key, depth + 1)?;
                array.set_index(scope, i, replaced);
            }
        }
        return Ok(value);
    }

    // Own enumerable string keys only — the same surface V8 serialized.
    let keys = match obj.get_own_property_names(scope, v8::GetPropertyNamesArgs::default()) {
        Some(k) => k,
        None => return Ok(value),
    };
    for i in 0..keys.length() {
        let Some(key) = keys.get_index(scope, i) else {
            continue;
        };
        let Some(item) = obj.get(scope, key) else {
            continue;
        };
        let replaced = rehydrate_at(scope, item, brand_key, depth + 1)?;
        if replaced != item {
            obj.set(scope, key, replaced);
        }
    }
    Ok(value)
}

fn brand_of(
    scope: &mut v8::PinScope,
    obj: v8::Local<v8::Object>,
    brand_key: v8::Local<v8::String>,
) -> Option<u32> {
    match obj.get(scope, brand_key.into()) {
        Some(v) if v.is_uint32() => v.uint32_value(scope),
        _ => None,
    }
}

fn descriptor_field<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    obj: v8::Local<v8::Object>,
    name: &str,
) -> Codec<v8::Local<'s, v8::Value>> {
    let Some(key) = v8::String::new(scope, name) else {
        return malformed("could not intern a descriptor key");
    };
    match obj.get(scope, key.into()) {
        Some(v) => Ok(v),
        None => malformed(&format!("descriptor is missing '{name}'")),
    }
}

/// The streamed body for a descriptor carrying a `bodyStream` handle, or
/// `None` when the descriptor's body is inline. Registers the stream in the
/// executing run's table and builds the sandbox-side stream object.
fn hydrated_stream_body<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    desc: v8::Local<v8::Object>,
) -> Codec<Option<v8::Local<'s, v8::Value>>> {
    let Some(key) = v8::String::new(scope, "bodyStream") else {
        return malformed("could not intern the bodyStream key");
    };
    // Own property only: a prototype-chain getter (guest-poisoned
    // Object.prototype) must not fabricate a handle on a buffered descriptor.
    // Self-harm either way, but cheap to close.
    if !desc.has_own_property(scope, key.into()).unwrap_or(false) {
        return Ok(None);
    }
    let Some(value) = desc.get(scope, key.into()) else {
        return Ok(None);
    };
    if !value.is_uint32() {
        return Ok(None);
    }
    let Some(stream_id) = value.uint32_value(scope) else {
        return Ok(None);
    };
    if !crate::v8::register_hydrated_stream(stream_id) {
        return Err(CodecError::Unsupported(
            "a streamed body can only be delivered to a running sandbox".to_string(),
        ));
    }
    match crate::v8::make_body_stream(scope, stream_id) {
        Some(stream) => Ok(Some(stream.into())),
        None => malformed("could not construct the body stream"),
    }
}

/// Turn one branded descriptor into a real instance.
///
/// Adding a type here plus a tag is the whole cost of making a new class cross
/// in this direction.
fn build_from_descriptor<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    desc: v8::Local<v8::Object>,
    tag: u32,
) -> Codec<v8::Local<'s, v8::Object>> {
    let headers_value = descriptor_field(scope, desc, "headers")?;
    let headers_list: v8::Local<v8::Array> = match headers_value.try_into() {
        Ok(a) => a,
        Err(_) => return malformed("descriptor headers is not an array"),
    };
    let flat = headers_list.length();
    if !flat.is_multiple_of(2) {
        return malformed("descriptor headers has an odd length");
    }
    if flat / 2 > MAX_HEADER_ENTRIES {
        return Err(CodecError::Unsupported(format!(
            "descriptor declares {} header entries, exceeding the {MAX_HEADER_ENTRIES} limit",
            flat / 2
        )));
    }
    let headers = match webtypes::make_headers(scope, headers_list) {
        Some(h) => h,
        None => return malformed("could not construct Headers"),
    };

    match tag {
        TAG_HEADERS => Ok(headers),
        TAG_REQUEST => {
            let url = descriptor_field(scope, desc, "url")?;
            let method = descriptor_field(scope, desc, "method")?;
            let body = match hydrated_stream_body(scope, desc)? {
                Some(stream) => stream,
                None => check_body(descriptor_field(scope, desc, "body")?)?,
            };
            match webtypes::make_request(scope, url, method, headers, body) {
                Some(r) => Ok(r),
                None => malformed("could not construct Request"),
            }
        }
        TAG_RESPONSE => {
            let status = descriptor_field(scope, desc, "status")?
                .uint32_value(scope)
                .unwrap_or(200);
            let status_text = descriptor_field(scope, desc, "statusText")?;
            let body = match hydrated_stream_body(scope, desc)? {
                Some(stream) => stream,
                None => check_body(descriptor_field(scope, desc, "body")?)?,
            };
            match webtypes::make_response(scope, status, status_text, headers, body) {
                Some(r) => Ok(r),
                None => malformed("could not construct Response"),
            }
        }
        other => Err(CodecError::Unsupported(format!(
            "received a {} descriptor (tag {other}); this build cannot materialise one",
            tag_name(other)
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blob;

    /// Fresh isolate + context with the web globals installed.
    fn with_web<R>(body: impl FnOnce(&mut v8::PinScope) -> R) -> R {
        crate::v8::init_platform();
        let isolate = &mut v8::Isolate::new(Default::default());
        v8::scope!(let scope, isolate);
        let context = v8::Context::new(scope, Default::default());
        let scope = &mut v8::ContextScope::new(scope, context);
        crate::webtypes::install(scope).expect("install web globals");
        body(scope)
    }

    fn eval<'s>(scope: &mut v8::PinScope<'s, '_>, src: &str) -> v8::Local<'s, v8::Value> {
        let s = v8::String::new(scope, src).unwrap();
        let script = v8::Script::compile(scope, s, None).expect("compile");
        script.run(scope).expect("run")
    }

    /// Serialize `expr`, read it back, bind the result to `__rt`, and evaluate
    /// `probe` against it. Exercises the real delegates end to end.
    fn round_trip(expr: &str, probe: &str) -> String {
        with_web(|scope| {
            let value = eval(scope, expr);
            let bytes = blob::serialize_value(scope, value).expect("serialize");
            let back = blob::deserialize_value(scope, &bytes).expect("deserialize");
            let global = scope.get_current_context().global(scope);
            let key = v8::String::new(scope, "__rt").unwrap();
            global.set(scope, key.into(), back);
            let out = eval(scope, probe);
            out.to_string(scope).unwrap().to_rust_string_lossy(scope)
        })
    }

    #[test]
    fn response_round_trips_as_a_real_instance() {
        let out = round_trip(
            "new Response('hello', { status: 201, statusText: 'Created', \
               headers: { 'content-type': 'text/plain' } })",
            "[__rt instanceof Response, __rt.status, __rt.statusText, \
              __rt.headers.get('content-type'), __rt.ok].join('|')",
        );
        assert_eq!(out, "true|201|Created|text/plain|true");
    }

    #[test]
    fn response_body_survives_as_a_string() {
        let out = round_trip(
            "new Response('{\"a\":1}', { headers: { 'content-type': 'application/json' } })",
            "__rt.json().then(v => v.a)",
        );
        // A resolved promise stringifies as [object Promise]; the value is
        // checked synchronously below instead.
        assert_eq!(out, "[object Promise]");

        let sync = round_trip("new Response('hi')", "typeof __rt.text");
        assert_eq!(sync, "function");
    }

    #[test]
    fn binary_body_survives_byte_for_byte() {
        let out = round_trip(
            "new Response(new Uint8Array([0, 1, 254, 255]))",
            "(async () => (await __rt.bytes()).join(','))()",
        );
        assert_eq!(out, "[object Promise]");

        // Read the body synchronously through the internal slot so the
        // assertion does not depend on draining microtasks.
        let bytes = round_trip(
            "new Response(new Uint8Array([0, 1, 254, 255]))",
            "[__rt._b.constructor.name, Array.from(__rt._b).join(',')].join('|')",
        );
        assert_eq!(bytes, "Uint8Array|0,1,254,255");
    }

    #[test]
    fn duplicate_set_cookie_survives() {
        let out = round_trip(
            "(() => { const r = new Response(null); \
               r.headers.append('set-cookie', 'a=1'); \
               r.headers.append('set-cookie', 'b=2'); return r })()",
            "JSON.stringify(__rt.headers.getSetCookie())",
        );
        assert_eq!(out, r#"["a=1","b=2"]"#);
    }

    #[test]
    fn request_round_trips_with_method_and_body() {
        let out = round_trip(
            "new Request('https://ex.com/api?q=1', { method: 'POST', body: 'payload', \
               headers: { 'x-custom-thing': 'v' } })",
            "[__rt instanceof Request, __rt.method, __rt.url, \
              __rt.headers.get('x-custom-thing'), __rt._b].join('|')",
        );
        assert_eq!(out, "true|POST|https://ex.com/api?q=1|v|payload");
    }

    #[test]
    fn headers_round_trip_standalone() {
        let out = round_trip(
            "new Headers([['content-type','a'],['x-odd','b']])",
            "[__rt instanceof Headers, __rt.get('content-type'), __rt.get('x-odd')].join('|')",
        );
        assert_eq!(out, "true|a|b");
    }

    #[test]
    fn host_types_round_trip_when_nested() {
        // The whole point of routing on internal fields rather than hand-
        // emitting: depth costs nothing.
        let out = round_trip(
            "({ outer: { list: [new Response('x', { status: 202 })] }, n: 7 })",
            "[__rt.n, __rt.outer.list[0] instanceof Response, \
              __rt.outer.list[0].status].join('|')",
        );
        assert_eq!(out, "7|true|202");
    }

    #[test]
    fn ordinary_values_are_untouched_by_the_delegate() {
        let out = round_trip(
            "({ d: new Date(1700000000000), m: new Map([['k', 1]]), b: 2n, \
                t: new Uint8Array([1,2]), s: new Set([3]) })",
            "[__rt.d.getTime(), __rt.m.get('k'), __rt.b, __rt.t.join(','), \
              [...__rt.s].join(',')].join('|')",
        );
        assert_eq!(out, "1700000000000|1|2|1,2|3");
    }

    #[test]
    fn round_trip_survives_deleted_and_shadowed_classes() {
        // Type identity is the construction-time private stamp and the class
        // references captured at install(), so neither serializing nor
        // deserializing depends on what guest code did to `globalThis`.
        // (Previously, deleting Response made an intact Response unserializable
        // and rewiring the prototype came from the guest-visible class.)
        let out = round_trip(
            "(() => { const r = new Response('x', { status: 201 }); \
               delete globalThis.Headers; globalThis.Response = class Fake {}; \
               return r })()",
            "[typeof __rt.text, __rt.status, __rt instanceof Response].join('|')",
        );
        assert_eq!(out, "function|201|false");
    }

    #[test]
    fn an_overridden_hasinstance_does_not_change_the_wire_tag() {
        // Guest code claiming `x instanceof Response === true` for everything
        // must not make a Headers serialize under the Response tag.
        let out = round_trip(
            "(() => { \
               Object.defineProperty(Response, Symbol.hasInstance, { value: () => true }); \
               return new Headers([['x-a','1']]) })()",
            "[__rt instanceof Headers, __rt.get('x-a')].join('|')",
        );
        assert_eq!(out, "true|1");
    }

    #[test]
    fn a_prototype_lookalike_crosses_as_plain_data() {
        // Re-pointing a prototype runs no constructor: no internal field, no
        // stamp. The object serializes as the plain data it is — a forged
        // "host type" cannot be minted from guest JS.
        let out = round_trip(
            "Object.setPrototypeOf({ _l: ['x-a', '1'] }, Headers.prototype)",
            "[__rt instanceof Headers, typeof __rt.get, Object.hasOwn(__rt, '_l')].join('|')",
        );
        assert_eq!(out, "false|undefined|true");
    }

    #[test]
    fn a_stream_body_is_refused_with_an_actionable_message() {
        let message = with_web(|scope| {
            // A duck-typed stream: bodyInit rejects it at construction time,
            // which is the earliest and clearest place to fail.
            let value = eval(
                scope,
                "(() => { try { \
                    new Response({ getReader() {} }); return 'constructed'; \
                  } catch (e) { return e.message } })()",
            );
            value.to_string(scope).unwrap().to_rust_string_lossy(scope)
        });
        assert!(
            message.contains("buffer it first"),
            "expected an actionable message, got: {message}"
        );
    }

    #[test]
    fn brand_key_is_the_prefix_plus_lowercase_hex() {
        let mut token = [0u8; DESCRIPTOR_TOKEN_LEN];
        token[0] = 0x00;
        token[1] = 0xff;
        token[15] = 0x1a;
        let key = brand_key_for_token(&token);
        assert_eq!(key, "__iso4_ht_00ff000000000000000000000000001a");
    }

    #[test]
    fn only_a_descriptor_stamped_with_the_session_key_rehydrates() {
        let key = brand_key_for_token(&[0xab; DESCRIPTOR_TOKEN_LEN]);
        let out = with_web(|scope| {
            // One descriptor stamped with the session key, one carrying the
            // bare prefix the way untrusted structured data might.
            let value = eval(
                scope,
                &format!(
                    "({{ real: {{ '{key}': 3, status: 201, statusText: '', \
                        headers: ['x-a','1'], body: null }}, \
                       fake: {{ __iso4_ht: 3, status: 500, statusText: '', \
                        headers: [], body: null }} }})"
                ),
            );
            let out = rehydrate(scope, value, &key).expect("rehydrate");
            let global = scope.get_current_context().global(scope);
            let name = v8::String::new(scope, "__rt").unwrap();
            global.set(scope, name.into(), out);
            let probe = eval(
                scope,
                "[__rt.real instanceof Response, __rt.real.status, \
                  __rt.real.headers.get('x-a'), \
                  __rt.fake instanceof Response, __rt.fake.__iso4_ht].join('|')",
            );
            probe.to_string(scope).unwrap().to_rust_string_lossy(scope)
        });
        assert_eq!(out, "true|201|1|false|3");
    }

    #[test]
    fn a_wrong_token_key_is_plain_data() {
        let session = brand_key_for_token(&[0xab; DESCRIPTOR_TOKEN_LEN]);
        let other = brand_key_for_token(&[0xcd; DESCRIPTOR_TOKEN_LEN]);
        let out = with_web(|scope| {
            let value = eval(
                scope,
                &format!("({{ '{other}': 1, headers: ['x-a','1'] }})"),
            );
            let out = rehydrate(scope, value, &session).expect("rehydrate");
            let global = scope.get_current_context().global(scope);
            let name = v8::String::new(scope, "__rt").unwrap();
            global.set(scope, name.into(), out);
            let probe = eval(scope, "__rt instanceof Headers");
            probe.to_string(scope).unwrap().to_rust_string_lossy(scope)
        });
        assert_eq!(out, "false");
    }

    #[test]
    fn the_byte_scan_needle_is_the_full_session_key() {
        let key = brand_key_for_token(&[0xab; DESCRIPTOR_TOKEN_LEN]);
        let stamped = format!("payload with {key} inside");
        assert!(might_contain_web_types(stamped.as_bytes(), &key));
        // The bare prefix — what untrusted data can contain — never triggers
        // the walk.
        assert!(!might_contain_web_types(
            b"payload with __iso4_ht inside",
            &key
        ));
        assert!(!might_contain_web_types(b"", &key));
        // Boundary positions and near-misses for the two-phase scan.
        assert!(might_contain_web_types(key.as_bytes(), &key));
        assert!(might_contain_web_types(format!("{key}x").as_bytes(), &key));
        assert!(might_contain_web_types(format!("x{key}").as_bytes(), &key));
        let truncated = &key[..key.len() - 1];
        assert!(!might_contain_web_types(truncated.as_bytes(), &key));
        // A first-byte hit too close to the end must not match or panic.
        assert!(!might_contain_web_types(
            format!("xxxx{truncated}").as_bytes(),
            &key
        ));
        // A failed candidate must not stop the scan before a later real match.
        assert!(might_contain_web_types(
            format!("__iso4_ht_zz{key}").as_bytes(),
            &key
        ));
    }

    #[test]
    fn an_oversized_header_set_is_refused_rather_than_allocated() {
        let err = with_web(|scope| {
            let value = eval(
                scope,
                &format!(
                    "(() => {{ const r = new Response(null); \
                       for (let i = 0; i < {}; i++) r.headers.append('x-h' + i, 'v'); \
                       return r }})()",
                    MAX_HEADER_ENTRIES + 1
                ),
            );
            blob::serialize_value(scope, value).expect_err("must refuse")
        });
        assert!(err.contains("exceeding the 1024 limit"), "got: {err}");
    }
}
