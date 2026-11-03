//! Thrown V8 values to `RunError`: capped message and stack text, syntax
//! errors, and the extra own fields a guest error carries to the host.

use std::sync::Arc;

use crate::blob;
use crate::errors::{self, RunError, RuntimeErrorData};
use crate::v8::isolate::ReasonCell;

/// Error text budget: leaves room for our marker under the wire clamp, so
/// only one marker, with the true length, reaches the host.
const ERROR_TEXT_BYTES: usize = errors::MAX_ERROR_TEXT_BYTES - 128;

/// Copy at most 64 Ki UTF-16 units, then at most `max_bytes` of UTF-8, of
/// guest text out of V8. `write_v2` reads a cons string in place;
/// `to_rust_string_lossy` would flatten it first.
pub(super) fn capped_text_lossy(
    scope: &v8::Isolate,
    text: v8::Local<v8::String>,
    max_bytes: usize,
) -> String {
    const MAX_UNITS: usize = 64 * 1024;
    let len = text.length();
    let mut out = if len <= MAX_UNITS {
        text.to_rust_string_lossy(scope)
    } else {
        let mut units = vec![0u16; MAX_UNITS];
        text.write_v2(scope, 0, &mut units, v8::WriteFlags::empty());
        // A cut inside a surrogate pair would decode to U+FFFD.
        if units.last().is_some_and(|u| (0xD800..0xDC00).contains(u)) {
            units.pop();
        }
        String::from_utf16_lossy(&units)
    };
    if len <= MAX_UNITS && out.len() <= max_bytes {
        return out;
    }
    out.truncate(out.floor_char_boundary(max_bytes));
    out.push_str(&format!("… [truncated by iso4: {len} UTF-16 units total]"));
    out
}

/// A compile failure is user code too: the pending `SyntaxError` as a run
/// error. Parse errors carry no frame, so the stack is built from V8's
/// message location — that is what names the file (`<prefix>`, a source
/// import, the run's own code).
pub(super) fn syntax_error(
    scope: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
) -> RunError {
    let Some(exception) = scope.exception() else {
        return RunError::Internal("compile failed without an exception".to_string());
    };
    let RunError::RuntimeError(mut data) = runtime_error_from_value(scope, exception) else {
        unreachable!("runtime_error_from_value always builds a RuntimeError");
    };
    // V8 gives a SyntaxError a header-only stack; add the frame it lacks.
    if !data.stack.as_deref().is_some_and(|s| s.contains("\n    at ")) {
        let location = scope.message().and_then(|m| {
            let file = m
                .get_script_resource_name(scope)
                .and_then(|n| n.to_string(scope))
                .map(|s| s.to_rust_string_lossy(scope))
                .unwrap_or_else(|| "<anonymous>".to_string());
            let line = m.get_line_number(scope)?;
            Some(format!("{file}:{line}:{}", m.get_start_column() + 1))
        });
        if let Some(location) = location {
            data.stack = Some(format!("{}: {}\n    at {location}", data.name, data.message));
        }
    }
    RunError::RuntimeError(data)
}

pub(super) fn runtime_error_from_value(
    scope: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
    value: v8::Local<v8::Value>,
) -> RunError {
    let name = error_name_from_value(scope, value).unwrap_or_else(|| "Error".to_string());
    let message = error_message_from_value(scope, value)
        .or_else(|| {
            value
                .to_string(scope)
                .map(|s| capped_text_lossy(scope, s, ERROR_TEXT_BYTES))
        })
        .unwrap_or_else(|| "JavaScript error".to_string());
    RunError::RuntimeError(Box::new(RuntimeErrorData {
        name,
        message,
        stack: stack_from_value(scope, value),
        fields: error_fields_from_value(scope, value),
    }))
}

pub(super) fn error_message_from_value(
    scope: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
    value: v8::Local<v8::Value>,
) -> Option<String> {
    let object = value.to_object(scope)?;
    let key = v8::String::new(scope, "message")?;
    let msg = object.get(scope, key.into())?;
    // Skip undefined/null - thrown primitives (strings, numbers) produce a
    // String wrapper object whose .message property is undefined, which would
    // stringify to the literal "undefined" instead of the thrown value.
    if msg.is_undefined() || msg.is_null() {
        return None;
    }
    msg.to_string(scope).map(|s| capped_text_lossy(scope, s, ERROR_TEXT_BYTES))
}

fn stack_from_value(
    scope: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
    value: v8::Local<v8::Value>,
) -> Option<String> {
    let object = value.to_object(scope)?;
    let key = v8::String::new(scope, "stack")?;
    let stack = object.get(scope, key.into())?;
    // Skip undefined/null — thrown primitives produce a wrapper object whose
    // .stack is undefined, which would stringify to the literal "undefined".
    if stack.is_undefined() || stack.is_null() {
        return None;
    }
    stack
        .to_string(scope)
        .map(|s| capped_text_lossy(scope, s, ERROR_TEXT_BYTES))
}

pub(super) fn exception_message(
    scope: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
) -> String {
    scope
        .exception()
        .and_then(|e| e.to_string(scope))
        .map(|s| capped_text_lossy(scope, s, ERROR_TEXT_BYTES))
        .unwrap_or_else(|| "(no exception message)".to_string())
}

pub(super) fn exception_stack(
    scope: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
) -> Option<String> {
    let exception = scope.exception()?;
    stack_from_value(scope, exception)
}

pub(super) fn error_name_from_value(
    scope: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
    value: v8::Local<v8::Value>,
) -> Option<String> {
    let obj = value.to_object(scope)?;
    let key = v8::String::new(scope, "name")?;
    let name_val = obj.get(scope, key.into())?;
    if !name_val.is_string() {
        return None;
    }
    name_val
        .to_string(scope)
        .map(|s| capped_text_lossy(scope, s, ERROR_TEXT_BYTES))
}

pub(super) fn exception_name(
    scope: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
) -> String {
    scope
        .exception()
        .and_then(|e| error_name_from_value(scope, e))
        .unwrap_or_else(|| "Error".to_string())
}

/// Keys that never travel in an error's `fields`: the first three are carried
/// in dedicated slots (and letting them through would let a payload spoof
/// Error identity or inject a fake stack), and `__proto__` is skipped so a
/// re-attached field can never land on the prototype accessor.
pub(super) const RESERVED_ERROR_KEYS: [&str; 4] = ["name", "message", "stack", "__proto__"];

/// Copy `source`'s own enumerable string-keyed properties onto `target`,
/// skipping `skip`. Uses `create_data_property` ([[DefineOwnProperty]]), so a
/// copied key is always a plain own data property.
pub(super) fn copy_own_properties_except(
    scope: &mut v8::PinScope,
    source: v8::Local<v8::Object>,
    target: v8::Local<v8::Object>,
    skip: &[&str],
) {
    let Some(names) = source.get_own_property_names(scope, v8::GetPropertyNamesArgs::default())
    else {
        return;
    };
    for i in 0..names.length() {
        let Some(key) = names.get_index(scope, i) else {
            continue;
        };
        let Some(name) = key.to_string(scope).map(|s| s.to_rust_string_lossy(scope)) else {
            continue;
        };
        if skip.contains(&name.as_str()) {
            continue;
        }
        // May invoke a throwing getter; a failed read simply drops the key.
        let Ok(property_key) = v8::Local::<v8::Name>::try_from(key) else {
            continue;
        };
        let Some(value) = source.get(scope, key) else {
            continue;
        };
        target.create_data_property(scope, property_key, value);
    }
}

/// Extract own enumerable properties from the thrown value as a value blob,
/// skipping [`RESERVED_ERROR_KEYS`]. Properties V8 refuses to clone
/// (functions, symbols, unresolved promises) are silently dropped. Returns
/// `None` for non-object throws or when no serializable own property remains.
pub(super) fn error_fields_from_value(
    scope: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
    value: v8::Local<v8::Value>,
) -> Option<Vec<u8>> {
    // Thrown primitives have no own properties of their own; to_object()
    // would create a wrapper (a String wrapper enumerates its character
    // indices) — never collect fields from those.
    if !value.is_object() {
        return None;
    }
    let obj = value.to_object(scope)?;
    let fields = v8::Object::new(scope);
    copy_own_properties_except(scope, obj, fields, &RESERVED_ERROR_KEYS);

    let names = fields.get_own_property_names(scope, v8::GetPropertyNamesArgs::default())?;
    if names.length() == 0 {
        return None;
    }
    if let Ok(bytes) = blob::serialize_value(scope, fields.into()) {
        return Some(bytes);
    }

    // One property is unserializable. Rebuild, testing each in isolation, so a
    // single function-valued field does not cost the caller every other one.
    let kept = v8::Object::new(scope);
    let mut any = false;
    for i in 0..names.length() {
        // A getter that ended the run gets no per-property retry.
        if scope
            .get_slot::<Arc<ReasonCell>>()
            .is_some_and(|reason| reason.get().is_some())
        {
            return None;
        }
        let Some(key) = names.get_index(scope, i) else {
            continue;
        };
        let Ok(property_key) = v8::Local::<v8::Name>::try_from(key) else {
            continue;
        };
        let Some(prop) = fields.get(scope, key) else {
            continue;
        };
        if blob::serialize_value(scope, prop).is_err() {
            continue;
        }
        kept.create_data_property(scope, property_key, prop);
        any = true;
    }
    if !any {
        return None;
    }
    blob::serialize_value(scope, kept.into()).ok()
}

pub(super) fn exception_fields(
    scope: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
) -> Option<Vec<u8>> {
    let exception = scope.exception()?;
    error_fields_from_value(scope, exception)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::v8::Limits;
    use crate::v8::testsupport::{get_default, run_code, run_err, run_ok};

    // ── Guest text copies ────────────────────────────────────────────────

    #[test]
    fn a_huge_thrown_message_is_capped_without_flattening() {
        // A 32 MB heap cannot hold the 64 MB string flattened: any flatten
        // would end the run with MemoryLimit instead.
        let failure = run_code(
            "throw new Error('a'.repeat(64 * 1024 * 1024))",
            "<iso4>",
            Limits {
                hard_memory_mb: 32,
                ..Default::default()
            },
        )
        .unwrap_err();
        let payload = failure.error.to_payload();
        let stack = payload.stack.as_deref().unwrap_or_default();
        // Message and stack each carry exactly one marker, with the true length.
        assert_eq!(payload.message.matches("[truncated by iso4: ").count(), 1);
        assert_eq!(stack.matches("[truncated by iso4: ").count(), 1);
        assert!(payload
            .message
            .ends_with("… [truncated by iso4: 67108864 UTF-16 units total]"));
        assert!(!stack.contains("67108864 UTF-16 units total"));
        // Both fit the wire cap, so the frontend's clamp adds no marker.
        for text in [payload.message.as_str(), stack] {
            assert!(
                matches!(errors::clamp_error_text(text), std::borrow::Cow::Borrowed(_)),
                "{}",
                text.len()
            );
        }
        assert_eq!(get_default(&run_ok("export default 1")).as_deref(), Some("1"));
    }

    #[test]
    fn a_text_cut_inside_a_surrogate_pair_leaves_no_replacement_char() {
        let err = run_err("throw new Error('a' + '😀'.repeat(40 * 1024))");
        let RunError::RuntimeError(data) = &err else {
            panic!("expected RuntimeError, got {err:?}");
        };
        assert!(!data.message.contains('\u{FFFD}'));
        assert!(data.message.ends_with("… [truncated by iso4: 81921 UTF-16 units total]"));
        assert!(data.message.len() <= errors::MAX_ERROR_TEXT_BYTES);
    }
}
