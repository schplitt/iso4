//! Test-only value tree for asserting on value blobs.
//!
//! Blobs are opaque bytes to Rust — reading one back needs a V8 isolate. The
//! unit tests in `v8.rs` assert on the *shape* of what crossed the boundary,
//! so this module gives them a plain Rust tree plus blob ⇄ tree conversions
//! that spin a throwaway isolate.
//!
//! This is deliberately **not** a codec: it exists only so tests can say
//! "the `count` export was the number 2" without hand-decoding V8's format.
//! Nothing outside `#[cfg(test)]` may use it.

use crate::blob;
use crate::v8::init_platform;

/// A decoded value, in the shape the tests reason about.
#[derive(Debug, Clone, PartialEq)]
pub enum TestValue {
    Undefined,
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    /// `sign` (true = negative) plus V8's little-endian 64-bit word array.
    BigInt(bool, Vec<u64>),
    /// A `Uint8Array`'s contents.
    Bytes(Vec<u8>),
    Array(Vec<TestValue>),
    /// Own enumerable string-keyed properties, in property order.
    Object(Vec<(String, TestValue)>),
    /// Anything the tests do not need structurally — a `Date`, `Map`, `Set`,
    /// `RegExp`, `Error`, non-`Uint8Array` typed array, … — rendered as a
    /// short description so a mismatch is still readable in a failure.
    Other(String),
    /// A back-reference to an object already on the current path. Blobs
    /// round-trip cycles, and a tree cannot hold one.
    Cycle,
}

/// Run `body` inside a fresh isolate + context.
fn with_scope<R>(body: impl FnOnce(&mut v8::PinScope) -> R) -> R {
    init_platform();
    let isolate = &mut v8::Isolate::new(Default::default());
    v8::scope!(let scope, isolate);
    let context = v8::Context::new(scope, Default::default());
    let scope = &mut v8::ContextScope::new(scope, context);
    body(scope)
}

/// Decode a value blob into a [`TestValue`] tree.
pub fn from_blob(bytes: &[u8]) -> TestValue {
    with_scope(|scope| {
        let value = blob::deserialize_value(scope, bytes)
            .unwrap_or_else(|| panic!("test blob failed to deserialize ({} bytes)", bytes.len()));
        read(scope, value, &mut Vec::new())
    })
}

/// Encode a [`TestValue`] tree as a value blob — the inverse of
/// [`from_blob`], for building `BridgeResponse` fixtures.
pub fn to_blob(value: &TestValue) -> Vec<u8> {
    with_scope(|scope| {
        let v8_value = build(scope, value);
        blob::serialize_value(scope, v8_value).expect("test value must serialize")
    })
}

/// Render a blob the way JS would print it — used where a test only needs a
/// human-readable comparison (dates, maps, class instances, …).
pub fn display_blob(bytes: &[u8]) -> String {
    match from_blob(bytes) {
        TestValue::Other(text) => text,
        other => format!("{other:?}"),
    }
}

fn read(
    scope: &mut v8::PinScope,
    value: v8::Local<v8::Value>,
    path: &mut Vec<v8::Global<v8::Value>>,
) -> TestValue {
    if value.is_undefined() {
        return TestValue::Undefined;
    }
    if value.is_null() {
        return TestValue::Null;
    }
    if value.is_boolean() {
        return TestValue::Bool(value.boolean_value(scope));
    }
    if value.is_number() {
        return TestValue::Number(value.number_value(scope).unwrap_or(f64::NAN));
    }
    if value.is_string() {
        return TestValue::String(value.to_rust_string_lossy(scope));
    }
    if value.is_big_int() {
        let bigint = v8::Local::<v8::BigInt>::try_from(value).unwrap();
        let mut buf = vec![0u64; bigint.word_count()];
        let (sign, filled) = bigint.to_words_array(&mut buf);
        return TestValue::BigInt(sign, filled.to_vec());
    }
    if value.is_uint8_array() {
        let view = v8::Local::<v8::ArrayBufferView>::try_from(value).unwrap();
        let mut buf = vec![0u8; view.byte_length()];
        let copied = view.copy_contents(&mut buf);
        buf.truncate(copied);
        return TestValue::Bytes(buf);
    }
    if value.is_array() || (value.is_object() && is_plain_object(scope, value)) {
        // Blobs round-trip cycles; a tree cannot, so a back-reference to
        // something already on the path becomes `Cycle`.
        for seen in path.iter() {
            let local = v8::Local::new(scope, seen);
            if value.strict_equals(local) {
                return TestValue::Cycle;
            }
        }
        path.push(v8::Global::new(scope, value));
        let decoded = if value.is_array() {
            let array = v8::Local::<v8::Array>::try_from(value).unwrap();
            let items = (0..array.length())
                .map(|i| match array.get_index(scope, i) {
                    Some(item) => read(scope, item, path),
                    None => TestValue::Undefined,
                })
                .collect();
            TestValue::Array(items)
        } else {
            let obj = value.to_object(scope).unwrap();
            let mut fields = Vec::new();
            if let Some(names) =
                obj.get_own_property_names(scope, v8::GetPropertyNamesArgs::default())
            {
                for i in 0..names.length() {
                    let Some(key) = names.get_index(scope, i) else {
                        continue;
                    };
                    let name = key.to_rust_string_lossy(scope);
                    let Some(prop) = obj.get(scope, key) else {
                        continue;
                    };
                    fields.push((name, read(scope, prop, path)));
                }
            }
            TestValue::Object(fields)
        };
        path.pop();
        return decoded;
    }
    TestValue::Other(describe(scope, value))
}

/// A "plain object" for test purposes: prototype is `Object.prototype` or
/// `null`. Everything else (Date, Map, Error, class instances, …) is `Other`.
fn is_plain_object(scope: &mut v8::PinScope, value: v8::Local<v8::Value>) -> bool {
    let Some(obj) = value.to_object(scope) else {
        return false;
    };
    let proto = obj.get_prototype(scope);
    match proto {
        Some(p) if p.is_null() => true,
        Some(p) => {
            let global = scope.get_current_context().global(scope);
            let object_key = v8::String::new(scope, "Object").unwrap().into();
            let prototype_key = v8::String::new(scope, "prototype").unwrap().into();
            let object_prototype = global
                .get(scope, object_key)
                .and_then(|o| o.to_object(scope))
                .and_then(|o| o.get(scope, prototype_key));
            object_prototype.is_some_and(|op| p.strict_equals(op))
        }
        None => false,
    }
}

/// JS-side description used for the `Other` variant.
fn describe(scope: &mut v8::PinScope, value: v8::Local<v8::Value>) -> String {
    let global = scope.get_current_context().global(scope);
    let key = v8::String::new(scope, "__describe").unwrap();
    global.set(scope, key.into(), value);
    let src = v8::String::new(
        scope,
        "(() => { const v = globalThis.__describe; \
           if (v instanceof Date) return `Date(${v.getTime()})`; \
           if (v instanceof RegExp) return `RegExp(${v.source}/${v.flags})`; \
           if (v instanceof Map) return `Map(${JSON.stringify([...v])})`; \
           if (v instanceof Set) return `Set(${JSON.stringify([...v])})`; \
           if (v instanceof Error) return `${v.name}(${v.message})`; \
           if (v instanceof ArrayBuffer) return `ArrayBuffer(${[...new Uint8Array(v)].join(',')})`; \
           if (v instanceof DataView) return `DataView(${[...new Uint8Array(v.buffer, v.byteOffset, v.byteLength)].join(',')})`; \
           if (ArrayBuffer.isView(v)) return `${v.constructor.name}(${[...v].join(',')})`; \
           return `${v?.constructor?.name ?? 'object'}(${JSON.stringify(v) ?? ''})`; \
         })()",
    )
    .unwrap();
    let script = v8::Script::compile(scope, src, None).unwrap();
    script
        .run(scope)
        .map(|v| v.to_rust_string_lossy(scope))
        .unwrap_or_else(|| "<undescribable>".to_string())
}

fn build<'s>(scope: &mut v8::PinScope<'s, '_>, value: &TestValue) -> v8::Local<'s, v8::Value> {
    match value {
        TestValue::Undefined => v8::undefined(scope).into(),
        TestValue::Null => v8::null(scope).into(),
        TestValue::Bool(b) => v8::Boolean::new(scope, *b).into(),
        TestValue::Number(n) => v8::Number::new(scope, *n).into(),
        TestValue::String(s) => v8::String::new(scope, s).unwrap().into(),
        TestValue::BigInt(sign, words) => v8::BigInt::new_from_words(scope, *sign, words)
            .unwrap()
            .into(),
        TestValue::Bytes(bytes) => {
            let len = bytes.len();
            let store = v8::ArrayBuffer::new_backing_store_from_vec(bytes.clone()).make_shared();
            let buffer = v8::ArrayBuffer::with_backing_store(scope, &store);
            v8::Uint8Array::new(scope, buffer, 0, len).unwrap().into()
        }
        TestValue::Array(items) => {
            let array = v8::Array::new(scope, items.len() as i32);
            for (i, item) in items.iter().enumerate() {
                let v = build(scope, item);
                array.set_index(scope, i as u32, v);
            }
            array.into()
        }
        TestValue::Object(fields) => {
            let obj = v8::Object::new(scope);
            for (key, val) in fields {
                let v = build(scope, val);
                let k = v8::String::new(scope, key).unwrap();
                obj.create_data_property(scope, k.into(), v);
            }
            obj.into()
        }
        TestValue::Other(text) => panic!("TestValue::Other({text}) cannot be re-encoded"),
        TestValue::Cycle => panic!("TestValue::Cycle cannot be re-encoded"),
    }
}
