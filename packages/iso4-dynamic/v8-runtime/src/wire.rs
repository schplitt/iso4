//! Binary wire codec for the iso4 Rust ↔ TypeScript protocol.
//!
//! Implements `WireValue` encoding/decoding per `docs/protocol.md` §4,
//! and `RunCompletionPayload` encoding per §5.6.
//!
//! Design notes:
//! - All integers are big-endian.
//! - `WireValue` is data-only: functions, Symbols, Promises are rejected at
//!   the extraction boundary in `v8.rs` before they reach this codec.
//! - The decoder (`decode_wire_value`) exists so that Rust tests can verify
//!   encode→decode roundtrips and that the TS decoder can be tested against
//!   identical byte vectors produced here.

use std::io;

use crate::v8::RunError;

// ── Value tags ─────────────────────────────────────────────────────────────────

const TAG_UNDEFINED: u8 = 0x00;
const TAG_NULL: u8 = 0x01;
const TAG_FALSE: u8 = 0x02;
const TAG_TRUE: u8 = 0x03;
const TAG_NUMBER: u8 = 0x04;
const TAG_STRING: u8 = 0x05;
const TAG_BIGINT: u8 = 0x06;
const TAG_BYTES: u8 = 0x07;
const TAG_ARRAY: u8 = 0x08;
const TAG_OBJECT: u8 = 0x09;

// ── WireValue ──────────────────────────────────────────────────────────────────

/// A data-only value that can cross the Rust ↔ TypeScript boundary.
///
/// Corresponds to `WireValue` in `docs/protocol.md` §4.
/// Functions, Symbols, and unresolved Promises are rejected before reaching
/// this type - only serializable data values are representable here.
#[derive(Debug, Clone, PartialEq)]
pub enum WireValue {
    Undefined,
    Null,
    Bool(bool),
    /// IEEE-754 double-precision float.
    Number(f64),
    String(String),
    /// Arbitrary-precision integer.
    ///
    /// `sign` is `true` for negative values. `words` is a little-endian
    /// sequence of 64-bit digits: `words[0]` holds bits 0–63, `words[1]`
    /// bits 64–127, etc. An empty `words` slice represents zero (sign is
    /// always `false` for zero). Matches V8's `new_from_words` / `to_words_array`
    /// representation exactly — no base conversion needed on either end.
    BigInt(bool, Vec<u64>),
    Bytes(Vec<u8>),
    Array(Vec<WireValue>),
    /// Ordered list of `(key, value)` pairs. Keys are own enumerable
    /// string-keyed properties; prototype methods are not included.
    Object(Vec<(String, WireValue)>),
}

// ── Primitive encoders ─────────────────────────────────────────────────────────

fn encode_u32(n: u32, out: &mut Vec<u8>) {
    out.extend_from_slice(&n.to_be_bytes());
}

fn encode_f64(n: f64, out: &mut Vec<u8>) {
    out.extend_from_slice(&n.to_be_bytes());
}

fn encode_string(s: &str, out: &mut Vec<u8>) {
    let bytes = s.as_bytes();
    encode_u32(bytes.len() as u32, out);
    out.extend_from_slice(bytes);
}

fn encode_bool(b: bool, out: &mut Vec<u8>) {
    out.push(u8::from(b));
}

fn encode_string_list(items: &[String], out: &mut Vec<u8>) {
    encode_u32(items.len() as u32, out);
    for s in items {
        encode_string(s, out);
    }
}

// ── WireValue encoder ──────────────────────────────────────────────────────────

/// Encode a single `WireValue` into `out`.
pub fn encode_wire_value(value: &WireValue, out: &mut Vec<u8>) {
    match value {
        WireValue::Undefined => out.push(TAG_UNDEFINED),
        WireValue::Null => out.push(TAG_NULL),
        WireValue::Bool(false) => out.push(TAG_FALSE),
        WireValue::Bool(true) => out.push(TAG_TRUE),
        WireValue::Number(n) => {
            out.push(TAG_NUMBER);
            encode_f64(*n, out);
        }
        WireValue::String(s) => {
            out.push(TAG_STRING);
            encode_string(s, out);
        }
        WireValue::BigInt(sign, words) => {
            out.push(TAG_BIGINT);
            out.push(u8::from(*sign));          // 0 = non-negative, 1 = negative
            encode_u32(words.len() as u32, out); // word_count
            for w in words {
                out.extend_from_slice(&w.to_be_bytes()); // each word big-endian
            }
        }
        WireValue::Bytes(b) => {
            out.push(TAG_BYTES);
            encode_u32(b.len() as u32, out);
            out.extend_from_slice(b);
        }
        WireValue::Array(items) => {
            out.push(TAG_ARRAY);
            encode_u32(items.len() as u32, out);
            for item in items {
                encode_wire_value(item, out);
            }
        }
        WireValue::Object(fields) => {
            out.push(TAG_OBJECT);
            encode_u32(fields.len() as u32, out);
            for (key, val) in fields {
                encode_string(key, out);
                encode_wire_value(val, out);
            }
        }
    }
}

// ── Primitive decoders ─────────────────────────────────────────────────────────

fn read_u8(data: &[u8], offset: &mut usize) -> io::Result<u8> {
    if *offset >= data.len() {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "unexpected end of WireValue data",
        ));
    }
    let b = data[*offset];
    *offset += 1;
    Ok(b)
}

fn read_u32(data: &[u8], offset: &mut usize) -> io::Result<u32> {
    let end = *offset + 4;
    if end > data.len() {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "unexpected end of WireValue data",
        ));
    }
    let n = u32::from_be_bytes(data[*offset..end].try_into().unwrap());
    *offset = end;
    Ok(n)
}

fn read_u64(data: &[u8], offset: &mut usize) -> io::Result<u64> {
    let end = *offset + 8;
    if end > data.len() {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "unexpected end of WireValue data",
        ));
    }
    let n = u64::from_be_bytes(data[*offset..end].try_into().unwrap());
    *offset = end;
    Ok(n)
}

fn read_f64(data: &[u8], offset: &mut usize) -> io::Result<f64> {
    let end = *offset + 8;
    if end > data.len() {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "unexpected end of WireValue data",
        ));
    }
    let n = f64::from_be_bytes(data[*offset..end].try_into().unwrap());
    *offset = end;
    Ok(n)
}

fn read_string(data: &[u8], offset: &mut usize) -> io::Result<String> {
    let len = read_u32(data, offset)? as usize;
    read_raw_bytes(data, offset, len).and_then(|bytes| {
        String::from_utf8(bytes).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "WireValue String is not valid UTF-8",
            )
        })
    })
}

fn read_raw_bytes(data: &[u8], offset: &mut usize, len: usize) -> io::Result<Vec<u8>> {
    let end = *offset + len;
    if end > data.len() {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "unexpected end of WireValue data",
        ));
    }
    let bytes = data[*offset..end].to_vec();
    *offset = end;
    Ok(bytes)
}

// ── WireValue decoder ──────────────────────────────────────────────────────────

/// Decode a single `WireValue` from `data` starting at `*offset`.
///
/// Advances `*offset` past all consumed bytes on success. On error the offset
/// value is unspecified and the caller should discard the entire buffer.
pub fn decode_wire_value(data: &[u8], offset: &mut usize) -> io::Result<WireValue> {
    let tag = read_u8(data, offset)?;
    match tag {
        TAG_UNDEFINED => Ok(WireValue::Undefined),
        TAG_NULL => Ok(WireValue::Null),
        TAG_FALSE => Ok(WireValue::Bool(false)),
        TAG_TRUE => Ok(WireValue::Bool(true)),
        TAG_NUMBER => {
            let n = read_f64(data, offset)?;
            Ok(WireValue::Number(n))
        }
        TAG_STRING => {
            let s = read_string(data, offset)?;
            Ok(WireValue::String(s))
        }
        TAG_BIGINT => {
            let sign = read_u8(data, offset)? != 0;
            let word_count = read_u32(data, offset)? as usize;
            let mut words = Vec::with_capacity(word_count);
            for _ in 0..word_count {
                words.push(read_u64(data, offset)?);
            }
            Ok(WireValue::BigInt(sign, words))
        }
        TAG_BYTES => {
            let len = read_u32(data, offset)? as usize;
            let bytes = read_raw_bytes(data, offset, len)?;
            Ok(WireValue::Bytes(bytes))
        }
        TAG_ARRAY => {
            let count = read_u32(data, offset)? as usize;
            let mut items = Vec::with_capacity(count);
            for _ in 0..count {
                items.push(decode_wire_value(data, offset)?);
            }
            Ok(WireValue::Array(items))
        }
        TAG_OBJECT => {
            let count = read_u32(data, offset)? as usize;
            let mut fields = Vec::with_capacity(count);
            for _ in 0..count {
                let key = read_string(data, offset)?;
                let val = decode_wire_value(data, offset)?;
                fields.push((key, val));
            }
            Ok(WireValue::Object(fields))
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unknown WireValue tag: {tag:#04x}"),
        )),
    }
}

// ── Completion payload types ───────────────────────────────────────────────────

/// Structured error fields for `RunCompletionPayload`.
pub struct RunErrorPayload {
    pub code: String,
    pub name: String,
    pub message: String,
    pub stack: Option<String>,
}

/// Payload for a successful run.
pub struct RunSuccessPayload {
    /// All exports as a flat `WireValue::Object`. The `default` export (if
    /// present) is encoded as the `"default"` key alongside named exports.
    pub exports: WireValue,
    pub stdout: Vec<String>,
    pub stderr: Vec<String>,
    pub duration_ms: f64,
}

/// Payload for a failed run.
pub struct RunFailurePayload {
    pub error: RunErrorPayload,
    pub stdout: Vec<String>,
    pub stderr: Vec<String>,
    pub duration_ms: f64,
}

/// The two variants of a completed run.
pub enum RunCompletion {
    Success(RunSuccessPayload),
    Failure(RunFailurePayload),
}

// ── RunCompletionPayload encoder ───────────────────────────────────────────────

/// Encode a `RunCompletionPayload` per `docs/protocol.md` §5.6.
///
/// Wire layout:
/// ```text
/// u32   runId
/// u8    ok  (1 = success, 0 = failure)
/// u8    successPresent  (1 when ok = 1)
///   WireValue  exports
///   List<String>  stdout
///   List<String>  stderr
///   f64  durationMs
/// u8    failurePresent  (1 when ok = 0)
///   RunErrorPayload  error
///   List<String>  stdout
///   List<String>  stderr
///   f64  durationMs
/// ```
pub fn encode_run_completion_payload(run_id: u32, completion: RunCompletion) -> Vec<u8> {
    let mut out = Vec::new();
    encode_u32(run_id, &mut out);

    match completion {
        RunCompletion::Success(s) => {
            encode_bool(true, &mut out);
            out.push(1); // Optional<RunSuccessPayload> present
            encode_wire_value(&s.exports, &mut out);
            encode_string_list(&s.stdout, &mut out);
            encode_string_list(&s.stderr, &mut out);
            encode_f64(s.duration_ms, &mut out);
            out.push(0); // Optional<RunFailurePayload> absent
        }
        RunCompletion::Failure(f) => {
            encode_bool(false, &mut out);
            out.push(0); // Optional<RunSuccessPayload> absent
            out.push(1); // Optional<RunFailurePayload> present
            encode_run_error_payload(&f.error, &mut out);
            encode_string_list(&f.stdout, &mut out);
            encode_string_list(&f.stderr, &mut out);
            encode_f64(f.duration_ms, &mut out);
        }
    }

    out
}

fn encode_run_error_payload(error: &RunErrorPayload, out: &mut Vec<u8>) {
    encode_string(&error.code, out);
    encode_string(&error.name, out);
    encode_string(&error.message, out);
    match &error.stack {
        Some(s) => {
            out.push(1);
            encode_string(s, out);
        }
        None => out.push(0),
    }
}

// ── PrecompileResultPayload encoder ──────────────────────────────────────────

/// Encode a `PrecompileResultPayload` per `docs/protocol.md` §5.6.
///
/// Wire layout:
/// ```text
/// u8    ok
/// u8    prefixIdPresent  (1 when ok = true)
///   String  prefixId
/// u8    errorPresent     (1 when ok = false)
///   RunErrorPayload  error
/// ```
pub fn encode_precompile_result_payload(
    prefix_id: Option<&str>,
    error: Option<&RunErrorPayload>,
) -> Vec<u8> {
    let mut out = Vec::new();
    match (prefix_id, error) {
        (Some(id), _) => {
            encode_bool(true, &mut out);
            out.push(1);
            encode_string(id, &mut out);
            out.push(0);
        }
        (_, Some(err)) => {
            encode_bool(false, &mut out);
            out.push(0);
            out.push(1);
            encode_run_error_payload(err, &mut out);
        }
        (None, None) => {
            unreachable!("encode_precompile_result_payload: must provide either prefix_id or error")
        }
    }
    out
}

// ── RunError → RunErrorPayload ─────────────────────────────────────────────────

/// Convert a V8 `RunError` into the wire-level `RunErrorPayload` per the error
/// code table in `docs/protocol.md` §7.
pub fn run_error_to_payload(error: &RunError) -> RunErrorPayload {
    match error {
        RunError::InvalidPayload(msg) => RunErrorPayload {
            code: "ERR_INTERNAL".to_string(),
            name: "Error".to_string(),
            message: msg.clone(),
            stack: None,
        },
        RunError::CompileError(msg) => RunErrorPayload {
            code: "ERR_COMPILE".to_string(),
            name: "SyntaxError".to_string(),
            message: msg.clone(),
            stack: None,
        },
        RunError::RuntimeError { message, stack } => RunErrorPayload {
            code: "ERR_USER_CODE".to_string(),
            name: "Error".to_string(),
            message: message.clone(),
            stack: stack.clone(),
        },
        RunError::ModuleNotFound(msg) => RunErrorPayload {
            code: "ERR_MODULE_NOT_FOUND".to_string(),
            name: "Error".to_string(),
            message: msg.clone(),
            stack: None,
        },
        RunError::ExportNotSerializable(msg) => RunErrorPayload {
            code: "ERR_EXPORT_NOT_SERIALIZABLE".to_string(),
            name: "Error".to_string(),
            message: msg.clone(),
            stack: None,
        },
        RunError::CpuTimeout => RunErrorPayload {
            code: "ERR_CPU_TIMEOUT".to_string(),
            name: "Error".to_string(),
            message: "CPU time limit exceeded".to_string(),
            stack: None,
        },
        RunError::WallTimeout => RunErrorPayload {
            code: "ERR_WALL_TIMEOUT".to_string(),
            name: "Error".to_string(),
            message: "Wall time limit exceeded".to_string(),
            stack: None,
        },
        RunError::MemoryLimit => RunErrorPayload {
            code: "ERR_MEMORY_LIMIT".to_string(),
            name: "Error".to_string(),
            message: "Memory limit exceeded".to_string(),
            stack: None,
        },
        RunError::HostBridge(msg) => RunErrorPayload {
            code: "ERR_HOST_BRIDGE".to_string(),
            name: "Error".to_string(),
            message: msg.clone(),
            stack: None,
        },
        RunError::UndeclaredBinding(msg) => RunErrorPayload {
            code: "ERR_UNDECLARED_BINDING".to_string(),
            name: "Error".to_string(),
            message: msg.clone(),
            stack: None,
        },
        RunError::FunctionArgumentNotSupported => RunErrorPayload {
            code: "ERR_FUNCTION_ARGUMENT_NOT_SUPPORTED".to_string(),
            name: "Error".to_string(),
            message: "function arguments are not supported across the bridge boundary".to_string(),
            stack: None,
        },
        RunError::BridgePayloadTooLarge => RunErrorPayload {
            code: "ERR_BRIDGE_PAYLOAD_TOO_LARGE".to_string(),
            name: "Error".to_string(),
            message: "bridge payload exceeds configured maxBridgePayloadBytes limit".to_string(),
            stack: None,
        },
        RunError::BridgeCallLimitExceeded => RunErrorPayload {
            code: "ERR_BRIDGE_CALL_LIMIT_EXCEEDED".to_string(),
            name: "Error".to_string(),
            message: "run exceeded the configured maxBridgeCalls limit".to_string(),
            stack: None,
        },
        RunError::Internal(msg) => RunErrorPayload {
            code: "ERR_INTERNAL".to_string(),
            name: "Error".to_string(),
            message: msg.clone(),
            stack: None,
        },
    }
}

// ── BridgeCall payload encoder ────────────────────────────────────────────────────

/// Encode a `BridgeCallPayload` per `docs/protocol.md` §5.4.
///
/// Wire layout:
/// ```text
/// u32                callId
/// u8                 targetKind  (0 = global, 1 = import)
/// Optional<String>   specifier   (present when targetKind = 1)
/// String             exportName
/// List<WireValue>    args
/// ```
pub fn encode_bridge_call_payload(
    call_id: u32,
    target_kind: u8, // 0 = global
    specifier: Option<&str>,
    export_name: &str,
    args: &[WireValue],
) -> Vec<u8> {
    let mut out = Vec::new();
    encode_u32(call_id, &mut out);
    out.push(target_kind);
    match specifier {
        Some(s) => { out.push(1); encode_string(s, &mut out); }
        None    => { out.push(0); }
    }
    encode_string(export_name, &mut out);
    encode_u32(args.len() as u32, &mut out);
    for arg in args {
        encode_wire_value(arg, &mut out);
    }
    out
}

// ── BridgeResponse payload decoder ───────────────────────────────────────────────

/// Decode a `BridgeResponsePayload` from TS per `docs/protocol.md` §5.4.
///
/// Returns `Ok(Ok(WireValue))` on success, `Ok(Err(message))` when the host
/// handler reported an error, and `Err(io::Error)` on a protocol fault.
///
/// Wire layout:
/// ```text
/// u32                callId
/// u8                 ok
/// Optional<WireValue> value   (present when ok = true)
/// Optional<error>    error   (present when ok = false)
///   String code  String name  String message  Optional<String> stack
/// ```
pub fn parse_bridge_response_payload(
    payload: &[u8],
) -> io::Result<(u32, Result<WireValue, String>)> {
    let mut offset = 0;

    // callId - returned to the caller for validation.  In v1 bridge calls are
    // sequential within a run, but the connection is reused across runs so a
    // stale BridgeResponse from a previous run's orphaned handler can arrive
    // here.  The caller compares this value against the callId it sent.
    let call_id = read_u32(payload, &mut offset)?;

    let ok_byte = read_u8(payload, &mut offset)?;
    match ok_byte {
        1 => {
            // ok = true - read Optional<WireValue>
            let present = read_u8(payload, &mut offset)?;
            let value = if present == 1 {
                decode_wire_value(payload, &mut offset)?
            } else {
                WireValue::Undefined
            };
            Ok((call_id, Ok(value)))
        }
        0 => {
            // ok = false - read the error payload: code name message stack
            let _code    = read_string(payload, &mut offset)?;
            let _name    = read_string(payload, &mut offset)?;
            let message  = read_string(payload, &mut offset)?;
            let stack_present = read_u8(payload, &mut offset)?;
            if stack_present == 1 {
                // Consume the stack string so the parser leaves at the end of
                // the payload. We don't use it — it's host-side context only.
                let _ = read_string(payload, &mut offset)?;
            }
            Ok((call_id, Err(message)))
        }
        b => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid BridgeResponse ok byte: {b:#04x}"),
        )),
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Encode a value and return the raw bytes.
    fn enc(v: &WireValue) -> Vec<u8> {
        let mut out = Vec::new();
        encode_wire_value(v, &mut out);
        out
    }

    /// Encode then decode a value and assert it round-trips cleanly.
    fn roundtrip(v: &WireValue) -> WireValue {
        let bytes = enc(v);
        let mut offset = 0;
        let decoded = decode_wire_value(&bytes, &mut offset).expect("decode failed");
        assert_eq!(offset, bytes.len(), "decoder did not consume all bytes");
        decoded
    }

    // ── Primitives ────────────────────────────────────────────────────────────

    #[test]
    fn undefined_encodes_to_single_tag_byte() {
        assert_eq!(enc(&WireValue::Undefined), vec![TAG_UNDEFINED]);
    }

    #[test]
    fn null_encodes_to_single_tag_byte() {
        assert_eq!(enc(&WireValue::Null), vec![TAG_NULL]);
    }

    #[test]
    fn false_encodes_to_single_tag_byte() {
        assert_eq!(enc(&WireValue::Bool(false)), vec![TAG_FALSE]);
    }

    #[test]
    fn true_encodes_to_single_tag_byte() {
        assert_eq!(enc(&WireValue::Bool(true)), vec![TAG_TRUE]);
    }

    #[test]
    fn number_encodes_as_tag_plus_8_byte_f64() {
        // 123.0 = 0x405EC00000000000 big-endian per docs/protocol.md §4.3
        let bytes = enc(&WireValue::Number(123.0));
        assert_eq!(bytes[0], TAG_NUMBER);
        assert_eq!(bytes.len(), 9);
        assert_eq!(&bytes[1..], &[0x40, 0x5e, 0xc0, 0x00, 0x00, 0x00, 0x00, 0x00]);
    }

    #[test]
    fn string_encodes_as_tag_u32_length_utf8_bytes() {
        let bytes = enc(&WireValue::String("some".to_string()));
        // TAG_STRING + u32(4) + "some"
        assert_eq!(
            bytes,
            vec![TAG_STRING, 0x00, 0x00, 0x00, 0x04, b's', b'o', b'm', b'e']
        );
    }

    #[test]
    fn bytes_encodes_as_tag_u32_length_raw_bytes() {
        let bytes = enc(&WireValue::Bytes(vec![0xde, 0xad, 0xbe, 0xef]));
        assert_eq!(
            bytes,
            vec![TAG_BYTES, 0x00, 0x00, 0x00, 0x04, 0xde, 0xad, 0xbe, 0xef]
        );
    }

    #[test]
    fn empty_array_encodes_as_tag_plus_zero_count() {
        let bytes = enc(&WireValue::Array(vec![]));
        assert_eq!(bytes, vec![TAG_ARRAY, 0x00, 0x00, 0x00, 0x00]);
    }

    #[test]
    fn empty_object_encodes_as_tag_plus_zero_count() {
        let bytes = enc(&WireValue::Object(vec![]));
        assert_eq!(bytes, vec![TAG_OBJECT, 0x00, 0x00, 0x00, 0x00]);
    }

    // ── Roundtrips ────────────────────────────────────────────────────────────

    #[test]
    fn undefined_roundtrips() {
        assert_eq!(roundtrip(&WireValue::Undefined), WireValue::Undefined);
    }

    #[test]
    fn null_roundtrips() {
        assert_eq!(roundtrip(&WireValue::Null), WireValue::Null);
    }

    #[test]
    fn bool_false_roundtrips() {
        assert_eq!(roundtrip(&WireValue::Bool(false)), WireValue::Bool(false));
    }

    #[test]
    fn bool_true_roundtrips() {
        assert_eq!(roundtrip(&WireValue::Bool(true)), WireValue::Bool(true));
    }

    #[test]
    fn number_roundtrips() {
        assert_eq!(roundtrip(&WireValue::Number(42.0)), WireValue::Number(42.0));
        assert_eq!(
            roundtrip(&WireValue::Number(3.14)),
            WireValue::Number(3.14)
        );
        assert_eq!(
            roundtrip(&WireValue::Number(-0.5)),
            WireValue::Number(-0.5)
        );
    }

    #[test]
    fn string_roundtrips() {
        assert_eq!(
            roundtrip(&WireValue::String("hello".to_string())),
            WireValue::String("hello".to_string())
        );
        assert_eq!(
            roundtrip(&WireValue::String(String::new())),
            WireValue::String(String::new())
        );
    }

    #[test]
    fn bytes_roundtrips() {
        assert_eq!(
            roundtrip(&WireValue::Bytes(vec![1, 2, 3])),
            WireValue::Bytes(vec![1, 2, 3])
        );
        assert_eq!(
            roundtrip(&WireValue::Bytes(vec![])),
            WireValue::Bytes(vec![])
        );
    }

    #[test]
    fn array_of_primitives_roundtrips() {
        let v = WireValue::Array(vec![
            WireValue::Number(1.0),
            WireValue::String("two".to_string()),
            WireValue::Bool(true),
            WireValue::Null,
        ]);
        assert_eq!(roundtrip(&v), v);
    }

    #[test]
    fn nested_object_roundtrips() {
        let v = WireValue::Object(vec![(
            "outer".to_string(),
            WireValue::Object(vec![(
                "inner".to_string(),
                WireValue::Number(99.0),
            )]),
        )]);
        assert_eq!(roundtrip(&v), v);
    }

    // ── Protocol example from docs/protocol.md §4.3 ───────────────────────────
    //
    // Sandbox code:  export const someExport = { hello: ['some', 123] }
    //
    // Expected byte-level layout for the `exports` WireValue:
    //
    //   09                                  # Object
    //   00 00 00 01                         # 1 field
    //   00 00 00 0a 73 6f 6d 65 45 78 70 6f 72 74  # key "someExport"
    //   09                                  # Object
    //   00 00 00 01                         # 1 field
    //   00 00 00 05 68 65 6c 6c 6f          # key "hello"
    //   08                                  # Array
    //   00 00 00 02                         # 2 items
    //   05                                  # String
    //   00 00 00 04 73 6f 6d 65             # "some"
    //   04                                  # Number
    //   40 5e c0 00 00 00 00 00             # f64 123.0

    #[test]
    fn protocol_example_nested_export_matches_spec_bytes() {
        let value = WireValue::Object(vec![(
            "someExport".to_string(),
            WireValue::Object(vec![(
                "hello".to_string(),
                WireValue::Array(vec![
                    WireValue::String("some".to_string()),
                    WireValue::Number(123.0),
                ]),
            )]),
        )]);

        #[rustfmt::skip]
        let expected: Vec<u8> = vec![
            0x09,                               // Object
            0x00, 0x00, 0x00, 0x01,             // 1 field
            // key "someExport" (10 bytes)
            0x00, 0x00, 0x00, 0x0a,
            b's', b'o', b'm', b'e', b'E', b'x', b'p', b'o', b'r', b't',
            0x09,                               // Object
            0x00, 0x00, 0x00, 0x01,             // 1 field
            // key "hello" (5 bytes)
            0x00, 0x00, 0x00, 0x05,
            b'h', b'e', b'l', b'l', b'o',
            0x08,                               // Array
            0x00, 0x00, 0x00, 0x02,             // 2 items
            0x05,                               // String
            0x00, 0x00, 0x00, 0x04,             // 4 bytes
            b's', b'o', b'm', b'e',
            0x04,                               // Number
            0x40, 0x5e, 0xc0, 0x00, 0x00, 0x00, 0x00, 0x00, // f64 123.0
        ];

        assert_eq!(enc(&value), expected);
    }

    #[test]
    fn protocol_example_nested_export_roundtrips() {
        let value = WireValue::Object(vec![(
            "someExport".to_string(),
            WireValue::Object(vec![(
                "hello".to_string(),
                WireValue::Array(vec![
                    WireValue::String("some".to_string()),
                    WireValue::Number(123.0),
                ]),
            )]),
        )]);
        assert_eq!(roundtrip(&value), value);
    }

    // Protocol example: default + named exports (§4.3)
    //
    //   export default { ok: true }
    //   export const count = 2
    //
    // Expected WireValue::Object with two keys: "default" and "count".

    #[test]
    fn protocol_example_default_and_named_exports_roundtrip() {
        let value = WireValue::Object(vec![
            (
                "default".to_string(),
                WireValue::Object(vec![("ok".to_string(), WireValue::Bool(true))]),
            ),
            ("count".to_string(), WireValue::Number(2.0)),
        ]);
        assert_eq!(roundtrip(&value), value);
    }

    // ── RunCompletionPayload ──────────────────────────────────────────────────

    #[test]
    fn success_payload_starts_with_run_id_and_ok_true() {
        let payload = encode_run_completion_payload(
            42,
            RunCompletion::Success(RunSuccessPayload {
                exports: WireValue::Object(vec![]),
                stdout: vec![],
                stderr: vec![],
                duration_ms: 1.0,
            }),
        );
        // First 4 bytes: run_id = 42
        assert_eq!(&payload[0..4], &42u32.to_be_bytes());
        // Byte 4: ok = 1
        assert_eq!(payload[4], 1);
        // Byte 5: success present = 1
        assert_eq!(payload[5], 1);
    }

    #[test]
    fn failure_payload_starts_with_run_id_and_ok_false() {
        let payload = encode_run_completion_payload(
            7,
            RunCompletion::Failure(RunFailurePayload {
                error: RunErrorPayload {
                    code: "ERR_COMPILE".to_string(),
                    name: "SyntaxError".to_string(),
                    message: "bad syntax".to_string(),
                    stack: None,
                },
                stdout: vec![],
                stderr: vec![],
                duration_ms: 0.5,
            }),
        );
        // First 4 bytes: run_id = 7
        assert_eq!(&payload[0..4], &7u32.to_be_bytes());
        // Byte 4: ok = 0
        assert_eq!(payload[4], 0);
        // Byte 5: success absent = 0
        assert_eq!(payload[5], 0);
        // Byte 6: failure present = 1
        assert_eq!(payload[6], 1);
    }

    #[test]
    fn success_payload_with_stdout_stderr_and_exports() {
        let exports = WireValue::Object(vec![("x".to_string(), WireValue::Number(1.0))]);
        let payload = encode_run_completion_payload(
            1,
            RunCompletion::Success(RunSuccessPayload {
                exports: exports.clone(),
                stdout: vec!["hello".to_string()],
                stderr: vec!["warn".to_string()],
                duration_ms: 2.5,
            }),
        );

        // Manually decode: skip run_id(4) + ok(1) + successPresent(1)
        let mut offset = 6;
        let decoded_exports = decode_wire_value(&payload, &mut offset).unwrap();
        assert_eq!(decoded_exports, exports);

        // stdout list: u32(1) + string "hello"
        let stdout_count = u32::from_be_bytes(payload[offset..offset + 4].try_into().unwrap());
        assert_eq!(stdout_count, 1);
        offset += 4;
        let hello_len = u32::from_be_bytes(payload[offset..offset + 4].try_into().unwrap());
        assert_eq!(hello_len, 5);
        offset += 4;
        assert_eq!(&payload[offset..offset + 5], b"hello");
    }

    // ── Decoder error cases ───────────────────────────────────────────────────

    #[test]
    fn decode_rejects_unknown_tag() {
        let bytes = vec![0xff];
        let mut offset = 0;
        let err = decode_wire_value(&bytes, &mut offset).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("0xff"));
    }

    #[test]
    fn decode_rejects_truncated_number() {
        // TAG_NUMBER but only 4 bytes of f64 instead of 8
        let bytes = vec![TAG_NUMBER, 0x00, 0x00, 0x00, 0x00];
        let mut offset = 0;
        let err = decode_wire_value(&bytes, &mut offset).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn decode_rejects_truncated_string_body() {
        // TAG_STRING, len=10, but only 3 bytes follow
        let bytes = vec![TAG_STRING, 0x00, 0x00, 0x00, 0x0a, b'a', b'b', b'c'];
        let mut offset = 0;
        let err = decode_wire_value(&bytes, &mut offset).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn decode_rejects_non_utf8_string() {
        // TAG_STRING, len=2, followed by invalid UTF-8 bytes
        let bytes = vec![TAG_STRING, 0x00, 0x00, 0x00, 0x02, 0xff, 0xfe];
        let mut offset = 0;
        let err = decode_wire_value(&bytes, &mut offset).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    // ── run_error_to_payload ──────────────────────────────────────────────────

    #[test]
    fn compile_error_maps_to_err_compile() {
        let payload = run_error_to_payload(&RunError::CompileError("bad syntax".to_string()));
        assert_eq!(payload.code, "ERR_COMPILE");
        assert_eq!(payload.name, "SyntaxError");
        assert!(payload.stack.is_none());
    }

    #[test]
    fn runtime_error_maps_to_err_user_code() {
        let payload = run_error_to_payload(&RunError::RuntimeError {
            message: "boom".to_string(),
            stack: Some("at line 1".to_string()),
        });
        assert_eq!(payload.code, "ERR_USER_CODE");
        assert_eq!(payload.stack, Some("at line 1".to_string()));
    }

    #[test]
    fn export_not_serializable_maps_to_err_export_not_serializable() {
        let payload =
            run_error_to_payload(&RunError::ExportNotSerializable("fn".to_string()));
        assert_eq!(payload.code, "ERR_EXPORT_NOT_SERIALIZABLE");
    }

    #[test]
    fn module_not_found_maps_to_err_module_not_found() {
        let payload = run_error_to_payload(&RunError::ModuleNotFound("lib:x".to_string()));
        assert_eq!(payload.code, "ERR_MODULE_NOT_FOUND");
    }

    #[test]
    fn cpu_timeout_maps_to_err_cpu_timeout() {
        let payload = run_error_to_payload(&RunError::CpuTimeout);
        assert_eq!(payload.code, "ERR_CPU_TIMEOUT");
    }

    #[test]
    fn memory_limit_maps_to_err_memory_limit() {
        let payload = run_error_to_payload(&RunError::MemoryLimit);
        assert_eq!(payload.code, "ERR_MEMORY_LIMIT");
    }
}
