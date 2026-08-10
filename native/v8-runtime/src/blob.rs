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
//! - Nothing here claims host objects. The delegates are deliberately empty so
//!   the bytes stay plain-V8 readable from Node (which cannot write custom
//!   host objects at all — see `V8_BLOB_FOLLOWUPS.md` §1).

use std::sync::OnceLock;

use v8::{ValueDeserializerHelper, ValueSerializerHelper};

/// Serializer delegate. Turns V8's data-clone refusal into a JS exception the
/// surrounding `TryCatch` picks up, so the caller gets the real message
/// ("function () {} could not be cloned") instead of a bare `None`.
struct SerDelegate;

impl v8::ValueSerializerImpl for SerDelegate {
    fn throw_data_clone_error<'s>(
        &self,
        scope: &mut v8::HandleScope<'s>,
        message: v8::Local<'s, v8::String>,
    ) {
        let exc = v8::Exception::error(scope, message);
        scope.throw_exception(exc);
    }
}

/// Deserializer delegate. Empty: we read no host objects (see module docs).
struct DeserDelegate;

impl v8::ValueDeserializerImpl for DeserDelegate {}

/// Serialize one V8 value into a blob.
///
/// Returns the V8 exception message on failure (functions, symbols, promises,
/// `WeakMap`, proxies). Callers wrap it in the typed `RunError` their boundary
/// uses, naming the offending export or argument.
pub fn serialize_value(
    scope: &mut v8::HandleScope,
    value: v8::Local<v8::Value>,
) -> Result<Vec<u8>, String> {
    let context = scope.get_current_context();
    let tc = &mut v8::TryCatch::new(scope);
    let serializer = v8::ValueSerializer::new(tc, Box::new(SerDelegate));
    serializer.write_header();
    if serializer.write_value(context, value) == Some(true) {
        return Ok(serializer.release());
    }
    let message = tc
        .exception()
        .and_then(|e| e.to_string(tc))
        .map(|s| s.to_rust_string_lossy(tc))
        .unwrap_or_else(|| "value could not be cloned".to_string());
    // The failure is reported through the returned Err; leaving the exception
    // pending would abort unrelated JS further up the stack.
    tc.reset();
    Err(message)
}

/// Read one V8 value back from a blob.
///
/// `None` means the bytes are truncated, corrupt, or written by a newer V8
/// serialization format than this binary can read. The format version is
/// checked once per connection at handshake time (see `session.rs`), so
/// reaching `None` at run time means a corrupt payload.
pub fn deserialize_value<'s>(
    scope: &mut v8::HandleScope<'s>,
    bytes: &[u8],
) -> Option<v8::Local<'s, v8::Value>> {
    let context = scope.get_current_context();
    let tc = &mut v8::TryCatch::new(scope);
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

// ── Handshake probe ──────────────────────────────────────────────────────────

/// V8 serialization header tag — the first byte of every blob.
pub const V8_BLOB_HEADER_TAG: u8 = 0xFF;

static PROBE: OnceLock<Vec<u8>> = OnceLock::new();

/// This binary's handshake probe: a serialized `null`.
///
/// Byte 0 is the header tag and byte 1 is the **format version** this V8
/// writes. Computed exactly once, in a throwaway isolate, so the session layer
/// never needs isolate plumbing: at handshake time it is a byte comparison.
pub fn probe() -> &'static [u8] {
    PROBE.get_or_init(|| {
        crate::v8::init_platform();
        let isolate = &mut v8::Isolate::new(Default::default());
        let scope = &mut v8::HandleScope::new(isolate);
        let context = v8::Context::new(scope, Default::default());
        let scope = &mut v8::ContextScope::new(scope, context);
        let null = v8::null(scope).into();
        serialize_value(scope, null).expect("serializing null must never fail")
    })
}

/// The V8 serialization format version this binary writes.
pub fn write_format_version() -> u8 {
    probe()[1]
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
    fn with_scope<R>(body: impl FnOnce(&mut v8::HandleScope) -> R) -> R {
        init_platform();
        let isolate = &mut v8::Isolate::new(Default::default());
        let scope = &mut v8::HandleScope::new(isolate);
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

    fn eval<'s>(scope: &mut v8::HandleScope<'s>, expr: &str) -> v8::Local<'s, v8::Value> {
        let src = v8::String::new(scope, expr).unwrap();
        let script = v8::Script::compile(scope, src, None).unwrap();
        script.run(scope).unwrap()
    }

    /// `JSON.stringify`-free stringification that survives Map/Set/BigInt.
    fn display(scope: &mut v8::HandleScope, value: v8::Local<v8::Value>) -> String {
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
        });
    }

    #[test]
    fn probe_format_version_rejects_non_blob_bytes() {
        assert_eq!(probe_format_version(&[]), None);
        assert_eq!(probe_format_version(&[0x01, 0x02]), None);
    }
}
