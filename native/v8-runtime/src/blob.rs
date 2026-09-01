//! Runtime-side value codec — V8 serialization blobs ("v8 blob").
//!
//! Every value crossing the Rust ↔ TypeScript boundary travels as a blob
//! produced by V8's own `ValueSerializer` (`docs/protocol.md` §4). The host
//! side uses Node's `v8.serialize`/`v8.deserialize` against the identical byte
//! format, so no hand-written codec sits between the two V8s.
//!
//! Two details are load-bearing and must not be "simplified":
//!
//! - `write_header()` must precede `write_value()`, and `read_header()` must
//!   precede `read_value()`. Skipping `read_header` does not fail cleanly — the
//!   version byte is then read as a value tag and every payload dies with a
//!   misleading host-object error.
//! - `has_custom_host_object` stays at its default `false`, and `is_host_object`
//!   is deliberately left unimplemented (its default throws). Returning `true`
//!   from `has_custom_host_object` makes V8 call the delegate for **every**
//!   plain object it serializes — a per-object cost on the bulk export path.
//!   Instead the sandbox's host types carry V8 internal fields, which V8
//!   dispatches on with a map field read and no callback at all. workerd relies
//!   on exactly the same property (`src/workerd/jsg/ser.c++`,
//!   `Serializer::HasCustomHostObject`). If a change ever makes V8 start
//!   calling `is_host_object`, the default impl throws and the failure is loud.
//!
//! Host objects (`Headers`, `Request`, `Response`) are encoded by `webcodec`;
//! this module only routes to it. Everything else stays plain-V8 readable from
//! Node.

use std::cell::RefCell;
use std::sync::OnceLock;

use v8::{ValueDeserializerHelper, ValueSerializerHelper};

use crate::webcodec::{self, CodecError};

thread_local! {
    /// Set by a delegate when it refuses a value, then taken by the surrounding
    /// call. V8's delegate methods can only signal failure as `None`/an
    /// exception, which loses the distinction between "unsupported type" and
    /// "malformed bytes" — and that distinction picks the error code.
    static LAST_CODEC_ERROR: RefCell<Option<CodecError>> = const { RefCell::new(None) };
}

/// Take the codec error recorded by the most recent serialize/deserialize, if
/// the failure came from a host type rather than from V8 itself.
pub fn take_codec_error() -> Option<CodecError> {
    LAST_CODEC_ERROR.with(|slot| slot.borrow_mut().take())
}

/// Record a codec failure **and throw**.
///
/// Returning `None` from a delegate is not enough. V8's
/// `ValueSerializer::WriteHostObject` propagates failure only when an exception
/// is already pending:
///
/// ```cpp
/// Maybe<bool> result = delegate_->WriteHostObject(...);
/// RETURN_VALUE_IF_EXCEPTION(isolate_, Nothing<bool>());
/// DCHECK(!result.IsNothing());   // no-op in release
/// ```
///
/// With no exception pending the `DCHECK` compiles away and serialization
/// reports **success** having written a truncated payload. Found by the
/// oversized-headers test, which passed serialization when it should have
/// failed.
fn record(scope: &mut v8::PinScope, error: CodecError) {
    let message =
        v8::String::new(scope, error.message()).unwrap_or_else(|| v8::String::empty(scope));
    let exception = v8::Exception::type_error(scope, message);
    scope.throw_exception(exception);
    LAST_CODEC_ERROR.with(|slot| *slot.borrow_mut() = Some(error));
}

fn clear_codec_error() {
    LAST_CODEC_ERROR.with(|slot| *slot.borrow_mut() = None);
}

/// Serializer delegate. Turns V8's data-clone refusal into a JS exception the
/// surrounding `TryCatch` picks up, so the caller gets the real message
/// ("function () {} could not be cloned") instead of a bare `None`.
struct SerDelegate;

impl v8::ValueSerializerImpl for SerDelegate {
    fn throw_data_clone_error<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        message: v8::Local<'s, v8::String>,
    ) {
        let exc = v8::Exception::error(scope, message);
        scope.throw_exception(exc);
    }

    /// Reached only for objects with internal fields — V8 routes them here
    /// without consulting `is_host_object`. See the module docs.
    fn write_host_object<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        object: v8::Local<'s, v8::Object>,
        value_serializer: &dyn ValueSerializerHelper,
    ) -> Option<bool> {
        let tag = match crate::webtypes::tag_of(scope, object) {
            Some(t) => t,
            None => {
                // An object with internal fields that is not one of ours. There
                // is no such object in this runtime today; if one appears, say
                // so rather than writing a malformed payload.
                record(
                    scope,
                    CodecError::Unsupported(
                        "value is a host object this build does not recognise".to_string(),
                    ),
                );
                return None;
            }
        };
        match webcodec::encode(scope, value_serializer, object, tag) {
            Ok(()) => Some(true),
            Err(e) => {
                record(scope, e);
                None
            }
        }
    }
}

/// Deserializer delegate. Materialises host objects through `webcodec`.
struct DeserDelegate;

impl v8::ValueDeserializerImpl for DeserDelegate {
    fn read_host_object<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        value_deserializer: &dyn ValueDeserializerHelper,
    ) -> Option<v8::Local<'s, v8::Object>> {
        match webcodec::decode(scope, value_deserializer) {
            Ok(obj) => Some(obj),
            Err(e) => {
                // Throwing is legal inside the DisallowJavascriptExecutionScope
                // that wraps ReadHostObject — raising an exception runs no JS.
                // workerd throws from the same place (`JSG_FAIL_REQUIRE`).
                record(scope, e);
                None
            }
        }
    }
}

/// Serialize one V8 value into a blob.
///
/// Returns the V8 exception message on failure (functions, symbols, promises,
/// `WeakMap`, proxies). Callers wrap it in the typed `RunError` their boundary
/// uses, naming the offending export or argument.
pub fn serialize_value(
    scope: &mut v8::PinScope,
    value: v8::Local<v8::Value>,
) -> Result<Vec<u8>, String> {
    clear_codec_error();
    let context = scope.get_current_context();
    v8::tc_scope!(let tc, scope);
    let serializer = v8::ValueSerializer::new(tc, Box::new(SerDelegate));
    serializer.write_header();
    if serializer.write_value(context, value) == Some(true) {
        let mut bytes = serializer.release();
        relabel_write_version(&mut bytes);
        return Ok(bytes);
    }
    // A host-type refusal carries a better message than V8's generic one, and
    // the caller needs it to pick between ERR_TYPE_NOT_SERIALIZABLE and
    // ERR_EXPORT_NOT_SERIALIZABLE. `take_codec_error` leaves it for them; this
    // only borrows the text.
    let message = LAST_CODEC_ERROR
        .with(|slot| slot.borrow().as_ref().map(|e| e.message().to_string()))
        .or_else(|| {
            tc.exception()
                .and_then(|e| e.to_string(tc))
                .map(|s| s.to_rust_string_lossy(tc))
        })
        .unwrap_or_else(|| "value could not be cloned".to_string());
    // The failure is reported through the returned Err; leaving the exception
    // pending would abort unrelated JS further up the stack.
    tc.reset();
    Err(message)
}

/// Read one V8 value back from a blob, materialising any host types the host
/// sent as stamped descriptors.
///
/// Use this on host → sandbox legs (data globals, bridge responses, call
/// args). Ordinary `deserialize_value` is for everything else and never walks.
///
/// Only descriptors carrying this session's brand key rehydrate (the key is
/// installed per thread from the handshake token — `webcodec`); anything else,
/// including data that merely looks branded, passes through as plain data.
/// The walk is guarded by a byte scan for the session key, so a payload with
/// no stamped descriptors pays only that scan.
pub fn deserialize_value_with_web_types<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    bytes: &[u8],
) -> Option<v8::Local<'s, v8::Value>> {
    let value = deserialize_value(scope, bytes)?;
    let Some(brand_key) = webcodec::session_brand_key() else {
        return Some(value);
    };
    if !webcodec::might_contain_web_types(bytes, &brand_key) {
        return Some(value);
    }
    match webcodec::rehydrate(scope, value, &brand_key) {
        Ok(v) => Some(v),
        Err(e) => {
            LAST_CODEC_ERROR.with(|slot| *slot.borrow_mut() = Some(e));
            None
        }
    }
}

/// Read one V8 value back from a blob.
///
/// `None` means the bytes are truncated, corrupt, or written by a newer V8
/// serialization format than this binary can read. The format version is
/// checked once per connection at handshake time (see `session.rs`), so
/// reaching `None` at run time means a corrupt payload.
pub fn deserialize_value<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    bytes: &[u8],
) -> Option<v8::Local<'s, v8::Value>> {
    clear_codec_error();
    let context = scope.get_current_context();
    v8::tc_scope!(let tc, scope);
    let deserializer = v8::ValueDeserializer::new(tc, Box::new(DeserDelegate), bytes);
    // read_header() BEFORE read_value() — see the module docblock.
    if deserializer.read_header(context) != Some(true) {
        tc.reset();
        return None;
    }
    let value = deserializer.read_value(context);
    if value.is_none() {
        tc.reset();
    }
    value
}

// ── Write-version relabel ────────────────────────────────────────────────────

/// The format version this binary's V8 natively writes (and the newest it can
/// read): 16 since V8 15.2.
const NATIVE_FORMAT_VERSION: u8 = 0x10;

/// The format version every blob leaves this process labelled with: 15, the
/// newest version all supported Node lines (22–26) can read.
const RELABELLED_FORMAT_VERSION: u8 = 0x0F;

/// Relabel a freshly serialized blob's header from format 16 to format 15.
///
/// V8 15.2 bumped the ValueSerializer format from 15 to 16 for ArrayBuffers
/// larger than 4 GB (buffer lengths became 64-bit varints), and no released
/// Node can read format 16. Below 4 GB the two byte streams are **identical**
/// except for the header's version byte — a varint encodes the same bytes for
/// any value under 2³² regardless of declared width — so rewriting that one
/// byte keeps every blob readable by Node 22–26 (and 27+, which read older
/// versions). Same mechanism Deno ships (denoland/deno#35118). The size
/// condition always holds here (frames are capped at 64 MiB), but is checked
/// anyway so an impossible oversized blob would keep the only header it can
/// satisfy rather than a corrupt version-15 label.
fn relabel_write_version(bytes: &mut [u8]) {
    if bytes.len() < 0x1_0000_0000
        && bytes.first() == Some(&V8_BLOB_HEADER_TAG)
        && bytes.get(1) == Some(&NATIVE_FORMAT_VERSION)
    {
        bytes[1] = RELABELLED_FORMAT_VERSION;
    }
}

// ── Handshake probe ──────────────────────────────────────────────────────────

/// V8 serialization header tag — the first byte of every blob.
pub const V8_BLOB_HEADER_TAG: u8 = 0xFF;

/// `(probe, read_version)` — the relabelled probe this binary advertises, and
/// the native format version its V8 can read up to.
static PROBE: OnceLock<(Vec<u8>, u8)> = OnceLock::new();

fn probe_data() -> &'static (Vec<u8>, u8) {
    PROBE.get_or_init(|| {
        crate::v8::init_platform();
        let isolate = &mut v8::Isolate::new(Default::default());
        v8::scope!(let scope, isolate);
        let context = v8::Context::new(scope, Default::default());
        let scope = &mut v8::ContextScope::new(scope, context);
        let null: v8::Local<v8::Value> = v8::null(scope).into();
        // Serialize raw (no relabel): byte 1 of the native output is the
        // format version this V8 writes — which is also the newest version
        // its deserializer accepts, i.e. the read ceiling for peer probes.
        let context_handle = scope.get_current_context();
        let raw = {
            v8::tc_scope!(let tc, scope);
            let serializer = v8::ValueSerializer::new(tc, Box::new(SerDelegate));
            serializer.write_header();
            assert_eq!(
                serializer.write_value(context_handle, null),
                Some(true),
                "serializing null must never fail"
            );
            serializer.release()
        };
        let read_version = raw[1];
        let mut probe = raw;
        relabel_write_version(&mut probe);
        (probe, read_version)
    })
}

/// This binary's handshake probe: a serialized `null`.
///
/// Byte 0 is the header tag and byte 1 is the **format version** this binary
/// writes (after the relabel, like every other blob it emits). Computed
/// exactly once, in a throwaway isolate, so the session layer never needs
/// isolate plumbing: at handshake time it is a byte comparison.
pub fn probe() -> &'static [u8] {
    &probe_data().0
}

/// The V8 serialization format version this binary writes — the relabelled
/// version carried by every emitted blob, not V8's native one.
pub fn write_format_version() -> u8 {
    probe()[1]
}

/// The newest V8 serialization format version this binary can read. Peer
/// probes up to this version are accepted at handshake, so a host whose V8
/// already writes the native format (Node 27+) still connects while the
/// relabel keeps our own blobs readable by older Node lines.
pub fn read_format_version() -> u8 {
    probe_data().1
}

/// Read the format version out of a peer's probe blob, or `None` when the
/// bytes are not a V8 serialization blob at all.
pub fn probe_format_version(probe: &[u8]) -> Option<u8> {
    match probe {
        [V8_BLOB_HEADER_TAG, version, ..] => Some(*version),
        _ => None,
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::v8::init_platform;

    /// Run `body` inside a fresh isolate + context.
    fn with_scope<R>(body: impl FnOnce(&mut v8::PinScope) -> R) -> R {
        init_platform();
        let isolate = &mut v8::Isolate::new(Default::default());
        v8::scope!(let scope, isolate);
        let context = v8::Context::new(scope, Default::default());
        let scope = &mut v8::ContextScope::new(scope, context);
        body(scope)
    }

    /// Evaluate `expr`, serialize the result, read it back, and stringify both
    /// the round-tripped value and the original for comparison.
    fn roundtrip_display(expr: &str) -> (String, String) {
        with_scope(|scope| {
            let value = eval(scope, expr);
            let before = display(scope, value);
            let bytes = serialize_value(scope, value).expect("serialize");
            let back = deserialize_value(scope, &bytes).expect("deserialize");
            let after = display(scope, back);
            (before, after)
        })
    }

    fn eval<'s>(scope: &mut v8::PinScope<'s, '_>, expr: &str) -> v8::Local<'s, v8::Value> {
        let src = v8::String::new(scope, expr).unwrap();
        let script = v8::Script::compile(scope, src, None).unwrap();
        script.run(scope).unwrap()
    }

    /// `JSON.stringify`-free stringification that survives Map/Set/BigInt.
    fn display(scope: &mut v8::PinScope, value: v8::Local<v8::Value>) -> String {
        let global = scope.get_current_context().global(scope);
        let key = v8::String::new(scope, "__display").unwrap();
        global.set(scope, key.into(), value);
        let out = eval(
            scope,
            "(() => { \
               const seen = new WeakSet(); \
               const show = (v) => { \
                 if (typeof v === 'bigint') return `bigint:${v}`; \
                 if (v instanceof Date) return `Date:${v.getTime()}`; \
                 if (v instanceof RegExp) return `RegExp:${v.source}/${v.flags}`; \
                 if (v instanceof Map) return `Map:[${[...v].map(([k, x]) => `${show(k)}=>${show(x)}`).join(',')}]`; \
                 if (v instanceof Set) return `Set:[${[...v].map(show).join(',')}]`; \
                 if (v instanceof Error) return `Error:${v.name}:${v.message}`; \
                 if (ArrayBuffer.isView(v)) return `${v.constructor.name}:[${[...v].join(',')}]`; \
                 if (v instanceof ArrayBuffer) return `ArrayBuffer:[${[...new Uint8Array(v)].join(',')}]`; \
                 if (v === null) return 'null'; \
                 if (typeof v !== 'object') return `${typeof v}:${String(v)}`; \
                 if (seen.has(v)) return '<cycle>'; \
                 seen.add(v); \
                 const body = Array.isArray(v) \
                   ? `[${v.map(show).join(',')}]` \
                   : `{${Object.keys(v).sort().map((k) => `${k}:${show(v[k])}`).join(',')}}`; \
                 seen.delete(v); \
                 return body; \
               }; \
               return show(globalThis.__display); \
             })()",
        );
        out.to_string(scope).unwrap().to_rust_string_lossy(scope)
    }

    #[test]
    fn primitives_roundtrip() {
        for expr in [
            "undefined",
            "null",
            "true",
            "false",
            "42",
            "-0.5",
            "'hello'",
            "''",
        ] {
            let (before, after) = roundtrip_display(expr);
            assert_eq!(before, after, "{expr}");
        }
    }

    #[test]
    fn bigint_roundtrips() {
        let (before, after) = roundtrip_display("2n ** 200n + 7n");
        assert_eq!(before, after);
        assert!(before.starts_with("bigint:"));
        let (before, after) = roundtrip_display("-(2n ** 90n)");
        assert_eq!(before, after);
    }

    #[test]
    fn date_map_set_regexp_error_roundtrip_as_real_instances() {
        for expr in [
            "new Date(1700000000000)",
            "new Map([['a', 1], [2, 'b']])",
            "new Set([1, 'two', 3])",
            "/ab+c/gi",
            "new TypeError('boom')",
        ] {
            let (before, after) = roundtrip_display(expr);
            assert_eq!(before, after, "{expr}");
        }
    }

    #[test]
    fn typed_arrays_and_subarray_windows_roundtrip() {
        for expr in [
            "new Uint8Array([1, 2, 3])",
            "new Uint8Array([1, 2, 3, 4, 5]).subarray(1, 4)",
            "new Float32Array([1.5, -2.5])",
            "new Int32Array([-1, 2 ** 30])",
            "new Uint8Array(0)",
            "new Uint8Array([9, 8, 7]).buffer",
        ] {
            let (before, after) = roundtrip_display(expr);
            assert_eq!(before, after, "{expr}");
        }
    }

    #[test]
    fn cycles_roundtrip() {
        let (before, after) =
            roundtrip_display("(() => { const o = { n: 1 }; o.self = o; return o })()");
        assert_eq!(before, after);
        assert!(after.contains("<cycle>"));
    }

    #[test]
    fn sparse_array_roundtrips() {
        let (before, after) = roundtrip_display("(() => { const a = [1]; a[50] = 2; return a })()");
        assert_eq!(before, after);
    }

    #[test]
    fn proto_own_key_passes_through_without_polluting() {
        let survives = with_scope(|scope| {
            let value = eval(
                scope,
                "Object.defineProperty({ x: 1 }, '__proto__', \
                 { value: { polluted: true }, enumerable: true, configurable: true })",
            );
            let bytes = serialize_value(scope, value).expect("serialize");
            let back = deserialize_value(scope, &bytes).expect("deserialize");
            let global = scope.get_current_context().global(scope);
            let key = v8::String::new(scope, "__rt").unwrap();
            global.set(scope, key.into(), back);
            let probe = eval(
                scope,
                "[Object.hasOwn(globalThis.__rt, '__proto__'), \
                  globalThis.__rt.x === 1, \
                  Object.getPrototypeOf(globalThis.__rt) === Object.prototype, \
                  ({}).polluted === undefined].join(',')",
            );
            probe.to_string(scope).unwrap().to_rust_string_lossy(scope)
        });
        assert_eq!(survives, "true,true,true,true");
    }

    #[test]
    fn functions_and_symbols_are_loud_errors() {
        with_scope(|scope| {
            for expr in [
                "() => {}",
                "Symbol('s')",
                "new WeakMap()",
                "new Promise(() => {})",
            ] {
                let value = eval(scope, expr);
                let err = serialize_value(scope, value).expect_err(expr);
                assert!(!err.is_empty(), "{expr}: empty error message");
            }
        });
    }

    #[test]
    fn deserialize_rejects_garbage() {
        with_scope(|scope| {
            assert!(deserialize_value(scope, &[]).is_none());
            assert!(deserialize_value(scope, &[0x00, 0x01, 0x02]).is_none());
            // Header tag with an impossible format version.
            assert!(deserialize_value(scope, &[V8_BLOB_HEADER_TAG, 0x63, 0x30]).is_none());
        });
    }

    #[test]
    fn probe_is_a_serialized_null_carrying_the_write_version() {
        let bytes = probe();
        assert_eq!(bytes[0], V8_BLOB_HEADER_TAG);
        assert_eq!(probe_format_version(bytes), Some(write_format_version()));
        with_scope(|scope| {
            let value = deserialize_value(scope, bytes).expect("probe deserializes");
            assert!(value.is_null());
            // The probe must be exactly what `serialize_value` emits for null
            // — the advertised write version is the relabelled one, not a
            // separately computed byte that could drift from real blobs.
            let null = v8::null(scope).into();
            assert_eq!(serialize_value(scope, null).unwrap(), bytes);
        });
    }

    /// Pins the read/write split this binary's Node-range compatibility rests
    /// on: V8 15.2 natively writes format 16, which no released Node reads, so
    /// every emitted blob is relabelled to 15 (`relabel_write_version`) while
    /// peer probes are accepted up to the native 16. If a V8 upgrade moves the
    /// native version past 16, this fails on purpose: re-verify that the new
    /// format is still byte-identical to 15 below 4 GB before re-pinning
    /// (denoland/deno#35118 was the evidence for 16).
    #[test]
    fn write_version_is_15_and_read_version_is_16() {
        assert_eq!(write_format_version(), 0x0F);
        assert_eq!(read_format_version(), 0x10);
    }

    #[test]
    fn every_emitted_blob_is_relabelled_to_format_15() {
        with_scope(|scope| {
            for expr in [
                "null",
                "42",
                "'hello'",
                "new Uint8Array([1, 2, 3]).buffer",
                "new Map([['k', new ArrayBuffer(16)]])",
                "2n ** 100n",
            ] {
                let value = eval(scope, expr);
                let bytes = serialize_value(scope, value).expect("serialize");
                assert_eq!(bytes[0], V8_BLOB_HEADER_TAG, "{expr}");
                assert_eq!(bytes[1], RELABELLED_FORMAT_VERSION, "{expr}");
                // This V8 still reads its own relabelled output.
                assert!(deserialize_value(scope, &bytes).is_some(), "{expr}");
            }
        });
    }

    /// The point of the relabel, proven against the real consumer: Node's
    /// `v8.deserialize` (format ceiling 15 on every released line) must read
    /// what this binary writes. Exercises ArrayBuffer lengths — the one field
    /// whose encoding the 15→16 bump changed. The e2e handshake proves this
    /// empirically per connection; this pins it at unit level with a value
    /// assertion, not just a successful decode.
    #[test]
    fn relabelled_blob_roundtrips_through_nodes_deserializer() {
        let hex = with_scope(|scope| {
            let value = eval(
                scope,
                "({ n: 42, buf: new Uint8Array([1, 2, 3]), big: 2n ** 100n, \
                   m: new Map([['k', 'v']]) })",
            );
            let bytes = serialize_value(scope, value).expect("serialize");
            bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
        });
        let output = std::process::Command::new("node")
            .arg("-e")
            .arg(
                "const v8 = require('node:v8'); \
                 const v = v8.deserialize(Buffer.from(process.argv[1], 'hex')); \
                 console.log(JSON.stringify([v.n, [...v.buf], v.big.toString(), v.m.get('k')]))",
            )
            .arg(&hex)
            .output()
            .expect("node must be on PATH (pnpm drives cargo test)");
        assert!(
            output.status.success(),
            "node rejected the blob: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            r#"[42,[1,2,3],"1267650600228229401496703205376","v"]"#
        );
    }

    #[test]
    fn probe_format_version_rejects_non_blob_bytes() {
        assert_eq!(probe_format_version(&[]), None);
        assert_eq!(probe_format_version(&[0x01, 0x02]), None);
    }

    #[test]
    fn descriptors_rehydrate_only_under_an_installed_session_key() {
        // Each #[test] runs on its own thread, so the thread-local starts
        // unset here: the first read must pass the descriptor through as
        // plain data (fail-closed), the second — after the key is installed —
        // must produce a real instance.
        crate::v8::init_platform();
        let isolate = &mut v8::Isolate::new(Default::default());
        v8::scope!(let scope, isolate);
        let context = v8::Context::new(scope, Default::default());
        let scope = &mut v8::ContextScope::new(scope, context);
        crate::webtypes::install(scope).expect("install web globals");

        let key = webcodec::brand_key_for_token(&[0xab; webcodec::DESCRIPTOR_TOKEN_LEN]);
        let value = eval(
            scope,
            &format!("({{ '{key}': 1, headers: ['x-a','1'] }})"),
        );
        let bytes = serialize_value(scope, value).expect("serialize");

        let plain = deserialize_value_with_web_types(scope, &bytes).expect("deserialize");
        let global = scope.get_current_context().global(scope);
        let name = v8::String::new(scope, "__rt").unwrap();
        global.set(scope, name.into(), plain);
        let probe = eval(scope, "String(__rt instanceof Headers)");
        assert_eq!(probe.to_rust_string_lossy(scope), "false");

        webcodec::set_session_brand_key(key);
        let instance = deserialize_value_with_web_types(scope, &bytes).expect("deserialize");
        global.set(scope, name.into(), instance);
        let probe = eval(
            scope,
            "[__rt instanceof Headers, __rt.get('x-a')].join('|')",
        );
        assert_eq!(probe.to_rust_string_lossy(scope), "true|1");
    }
}
