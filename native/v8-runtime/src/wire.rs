//! Frame payload encoders/decoders for the iso4 Rust ↔ TypeScript protocol.
//!
//! This module owns the **envelope**: run completion payloads, precompile
//! results, and bridge call/response payloads. The types it encodes live in
//! `types.rs`; the error code table is `errors.rs`. It is
//! deliberately value-codec-free — every value slot is `u32 byteLength` + a V8
//! serialization blob (see `blob.rs` and `docs/protocol.md` §4), carried here
//! as opaque bytes because materialising one needs a V8 isolate.
//!
//! Design notes:
//! - All integers are big-endian.
//! - The strings in these payloads (`callId`, `exportName`, error `name` /
//!   `message`, …) are the control plane; blobs are the data plane.

use std::io;

use crate::errors::clamp_error_text;
use crate::types::{
    BridgeCallRecord, BridgeErrorPayload, BridgeTarget, RunCompletion, RunErrorPayload,
};

// ── Primitive encoders ─────────────────────────────────────────────────────────

fn encode_u32(n: u32, out: &mut Vec<u8>) {
    out.extend_from_slice(&n.to_be_bytes());
}

fn encode_u64(n: u64, out: &mut Vec<u8>) {
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

/// Write a value slot: `u32 byteLength` + the V8 serialization blob.
fn encode_value_blob(blob: &[u8], out: &mut Vec<u8>) {
    encode_u32(blob.len() as u32, out);
    out.extend_from_slice(blob);
}

/// Write an `Optional<value slot>`: a presence byte, then the slot when set.
fn encode_optional_value_blob(blob: Option<&Vec<u8>>, out: &mut Vec<u8>) {
    match blob {
        Some(b) => {
            out.push(1);
            encode_value_blob(b, out);
        }
        None => out.push(0),
    }
}

// ── Primitive decoders ─────────────────────────────────────────────────────────

fn read_u8(data: &[u8], offset: &mut usize) -> io::Result<u8> {
    if *offset >= data.len() {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "unexpected end of payload data",
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
            "unexpected end of payload data",
        ));
    }
    let n = u32::from_be_bytes(data[*offset..end].try_into().unwrap());
    *offset = end;
    Ok(n)
}

fn read_string(data: &[u8], offset: &mut usize) -> io::Result<String> {
    let len = read_u32(data, offset)? as usize;
    read_raw_bytes(data, offset, len).and_then(|bytes| {
        String::from_utf8(bytes).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "payload string is not valid UTF-8",
            )
        })
    })
}

fn read_raw_bytes(data: &[u8], offset: &mut usize, len: usize) -> io::Result<Vec<u8>> {
    let end = offset
        .checked_add(len)
        .filter(|e| *e <= data.len())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "unexpected end of payload data",
            )
        })?;
    let bytes = data[*offset..end].to_vec();
    *offset = end;
    Ok(bytes)
}

/// Read a value slot: `u32 byteLength` + V8 serialization blob.
fn read_value_blob(data: &[u8], offset: &mut usize) -> io::Result<Vec<u8>> {
    let len = read_u32(data, offset)? as usize;
    read_raw_bytes(data, offset, len)
}

// ── Reset cause byte ─────────────────────────────────────────────────────────

fn reset_cause_byte(cause: crate::v8::ResetCause) -> u8 {
    match cause {
        crate::v8::ResetCause::Cpu => 0,
        crate::v8::ResetCause::Memory => 1,
        crate::v8::ResetCause::Abort => 2,
        crate::v8::ResetCause::Internal => 3,
        crate::v8::ResetCause::Wall => 4,
        crate::v8::ResetCause::Unsupported => 5,
    }
}

// ── RunCompletionPayload encoder ───────────────────────────────────────────────

/// Encode a `RunCompletionPayload` per `docs/protocol.md` §5.6.
///
/// Wire layout:
/// ```text
/// u32   runId
/// u32   slotAllowance  (runs the host may keep in flight; 0 = no opinion)
/// u8    ok  (1 = success, 0 = failure)
/// u8    successPresent  (1 when ok = 1)
///   ValueBlob  exports
///   List<String>  skippedExports
///   List<String>  stdout
///   List<String>  stderr
///   f64  durationMs
///   f64  wallTimeMs
///   f64  cpuTimeMs
///   List<BridgeCallRecord>  bridgeCalls
///   Optional<u64>  heapUsedBytes   (present for prefix runs)
///   u8   backgroundFlags            (bit0 waitUntil, bit1 streams; nonzero
///                                    = a RunComplete frame follows)
/// u8    failurePresent  (1 when ok = 0)
///   RunErrorPayload  error
///   List<String>  stdout
///   List<String>  stderr
///   f64  durationMs
///   f64  wallTimeMs
///   f64  cpuTimeMs
///   List<BridgeCallRecord>  bridgeCalls
///   Optional<u64>  heapUsedBytes   (present for prefix runs)
/// ```
pub fn encode_run_completion_payload(run_id: u32, completion: RunCompletion) -> Vec<u8> {
    let mut out = Vec::new();
    encode_u32(run_id, &mut out);
    // The runtime's current concurrency allowance, on every completion:
    // four bytes on a frame the host already reads, instead of a poll or a
    // second channel. Zero = no opinion yet (before the cold-start seed).
    encode_u32(crate::warm::slot_allowance(), &mut out);

    match completion {
        RunCompletion::Success(s) => {
            encode_bool(true, &mut out);
            out.push(1); // Optional<RunSuccessPayload> present
            encode_value_blob(&s.exports, &mut out);
            encode_string_list(&s.skipped_exports, &mut out);
            encode_string_list(&s.stdout, &mut out);
            encode_string_list(&s.stderr, &mut out);
            encode_f64(s.duration_ms, &mut out);
            encode_f64(s.wall_time_ms, &mut out);
            encode_f64(s.cpu_time_ms, &mut out);
            encode_bridge_call_records(&s.bridge_calls, &mut out);
            encode_optional_u64(s.heap_used_bytes, &mut out);
            out.push(s.background_flags);
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
            encode_f64(f.wall_time_ms, &mut out);
            encode_f64(f.cpu_time_ms, &mut out);
            encode_bridge_call_records(&f.bridge_calls, &mut out);
            encode_optional_u64(f.heap_used_bytes, &mut out);
        }
    }

    out
}

/// Encode a `RunCompletePayload` — the final frame of a run whose
/// Result reported pending background work.
///
/// Wire layout:
/// ```text
/// u32   runId
/// u8    status  (0 = settled, 1 = truncated, 2 = failed)
/// f64   durationMs      (grace wall time after the Result)
/// f64   wallTimeMs      (counted grace logic time)
/// f64   cpuTimeMs       (active V8 time during grace)
/// List<String>  stdout  (console lines written during grace)
/// List<String>  stderr
/// List<BridgeCallRecord>  bridgeCalls  (attempted during grace)
/// Optional<(String name, String message)>  error
/// ```
pub fn encode_run_complete_payload(run_id: u32, report: &crate::v8::GraceReport) -> Vec<u8> {
    let mut out = Vec::new();
    encode_u32(run_id, &mut out);
    out.push(match report.status {
        crate::v8::GraceStatus::Settled => 0,
        crate::v8::GraceStatus::Truncated => 1,
        crate::v8::GraceStatus::Failed => 2,
    });
    encode_f64(report.duration_ms, &mut out);
    encode_f64(report.wall_time_ms, &mut out);
    encode_f64(report.cpu_time_ms, &mut out);
    encode_string_list(&report.stdout, &mut out);
    encode_string_list(&report.stderr, &mut out);
    encode_bridge_call_records(&report.bridge_calls, &mut out);
    match &report.error {
        Some((name, message)) => {
            out.push(1);
            encode_string(name, &mut out);
            encode_string(message, &mut out);
        }
        None => out.push(0),
    }
    out
}

fn encode_optional_u64(value: Option<u64>, out: &mut Vec<u8>) {
    match value {
        Some(v) => {
            out.push(1);
            encode_u64(v, out);
        }
        None => out.push(0),
    }
}

/// Encode `List<BridgeCallRecord>`. Per record:
/// ```text
/// String        name
/// f64           startMs
/// f64           durationMs
/// u32           argBytes
/// u32           responseBytes
/// u32           callId
/// u8            outcome (0 ok, 1 blocked, 2 error, 3 unanswered)
/// ```
fn encode_bridge_call_records(records: &[BridgeCallRecord], out: &mut Vec<u8>) {
    encode_u32(records.len() as u32, out);
    for r in records {
        encode_string(&r.name, out);
        encode_f64(r.start_ms, out);
        encode_f64(r.duration_ms, out);
        encode_u32(r.arg_bytes, out);
        encode_u32(r.response_bytes, out);
        encode_u32(r.call_id, out);
        out.push(r.outcome as u8);
    }
}

/// Ceiling on the serialized own-properties blob attached to an error. Same
/// reasoning as [`crate::errors::MAX_ERROR_TEXT_BYTES`], but this one is a value blob and
/// cannot be truncated meaningfully, so an oversized one is dropped whole.
const MAX_ERROR_FIELDS_BYTES: usize = 1024 * 1024;

fn encode_run_error_payload(error: &RunErrorPayload, out: &mut Vec<u8>) {
    encode_string(&error.code, out);
    encode_string(&error.name, out);
    encode_string(&clamp_error_text(&error.message), out);
    match &error.stack {
        Some(s) => {
            out.push(1);
            encode_string(&clamp_error_text(s), out);
        }
        None => out.push(0),
    }
    // The code and name always survive, so the caller still learns what kind of
    // failure this was even when the detail cannot be carried.
    let fields = error.fields.as_ref().filter(|f| {
        let keep = f.len() <= MAX_ERROR_FIELDS_BYTES;
        if !keep {
            eprintln!(
                "[iso4-v8] dropping {} bytes of error properties (limit {MAX_ERROR_FIELDS_BYTES})",
                f.len()
            );
        }
        keep
    });
    encode_optional_value_blob(fields, out);
    match &error.reset {
        Some(info) => {
            out.push(1);
            out.push(reset_cause_byte(info.cause));
            encode_u32(info.culprit_run_id, out);
        }
        None => out.push(0),
    }
    out.push(u8::from(error.prefix_stage));
    match &error.cause {
        Some(cause) => {
            out.push(1);
            encode_string(&cause.code, out);
            encode_string(&cause.name, out);
            encode_string(&clamp_error_text(&cause.message), out);
            match &cause.stack {
                Some(s) => {
                    out.push(1);
                    encode_string(&clamp_error_text(s), out);
                }
                None => out.push(0),
            }
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
    request_id: u32,
    prefix_id: Option<&str>,
    error: Option<&RunErrorPayload>,
) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&request_id.to_be_bytes());
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

// ── StatsPayload encoder ─────────────────────────────────────────────────────

/// Encode a `StatsPayload` per `docs/protocol.md` §5.7 — the reply to a
/// `Stats` request.
///
/// Wire layout:
/// ```text
/// u32   oneoffRunning
/// u32   warmBusy
/// u32   warmIdle
/// u64   idleHeapBytes
/// u64   warmBudgetBytes (the RSS mark; 0 = watermarks disabled)
/// u64   rssBytes        (0 = unreadable)
/// u8    underPressure   (1 = shedding latch is held)
/// u32   prefixCount, then per prefix:
///   String  prefixId
///   u32     idle
///   u32     busy
/// ```
pub fn encode_stats_payload(stats: &crate::warm::RegistryStats) -> Vec<u8> {
    // Counts saturate at u32::MAX instead of wrapping: a live cap that is a
    // multiple of 2^32 (seen with cgroup v1's "unlimited" memory sentinel
    // before the host clamped its budget) would otherwise truncate to 0.
    fn encode_count(n: usize, out: &mut Vec<u8>) {
        encode_u32(u32::try_from(n).unwrap_or(u32::MAX), out);
    }
    let mut out = Vec::new();
    encode_count(stats.oneoff_running, &mut out);
    encode_count(stats.warm_busy, &mut out);
    encode_count(stats.warm_runs, &mut out);
    encode_count(stats.warm_idle, &mut out);
    encode_u64(stats.idle_heap_bytes, &mut out);
    encode_u64(stats.warm_budget_bytes, &mut out);
    encode_u64(stats.rss_bytes, &mut out);
    encode_u64(stats.usage_bytes, &mut out);
    encode_u64(stats.hard_line_bytes, &mut out);
    out.push(u8::from(stats.under_pressure));
    encode_count(stats.per_prefix.len(), &mut out);
    for (prefix_id, idle, busy) in &stats.per_prefix {
        encode_string(prefix_id, &mut out);
        encode_count(*idle, &mut out);
        encode_count(*busy, &mut out);
    }
    out
}

// ── BridgeCall payload encoder ────────────────────────────────────────────────────

/// Encode a `BridgeCallPayload` per `docs/protocol.md` §5.4.
///
/// Wire layout:
/// ```text
/// u32                runId       (the owning run — leads every run-scoped frame)
/// u32                callId
/// u8                 targetKind  (0 = global, 1 = import)
/// Optional<String>   specifier   (present when targetKind = 1)
/// String             exportName  (targetKind = 0)
///   | List<String>   path        (targetKind = 1: the leaf's key segments)
/// ValueBlob         args   (one blob holding the whole argument array)
/// ```
/// Everything in that layout except the argument bytes themselves, ending
/// with the blob's `u32` length prefix.
///
/// The arguments are already a serialized buffer by the time this runs, and
/// they are the only part of the payload with no bound: the frontend writes
/// this header and then that buffer in one vectored write instead of
/// concatenating them.
pub fn encode_bridge_call_header(
    run_id: u32,
    call_id: u32,
    target: &BridgeTarget,
    args_blob_len: u32,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(64);
    // The owning run leads the frame, like every other run-scoped frame:
    // handlers are per run, and the host cannot map callId → run on its own
    // (call ids are allocated child-side). 0 = a direct-API run with no wire
    // identity.
    encode_u32(run_id, &mut out);
    encode_u32(call_id, &mut out);
    match target {
        BridgeTarget::Global(name) => {
            out.extend_from_slice(&[0, 0]); // targetKind, specifier absent
            encode_string(name, &mut out);
        }
        BridgeTarget::Import { specifier, path } => {
            out.extend_from_slice(&[1, 1]); // targetKind, specifier present
            encode_string(specifier, &mut out);
            encode_string_list(path, &mut out);
        }
    }
    encode_u32(args_blob_len, &mut out);
    out
}

// ── BridgeResponse payload decoder ───────────────────────────────────────────────

/// Decode a `BridgeResponsePayload` from TS per `docs/protocol.md` §5.4.
///
/// Returns `Ok(Ok(Some(blob)))` on success, `Ok(Ok(None))` when the handler
/// returned nothing (the value slot is absent → `undefined`),
/// `Ok(Err(BridgeErrorPayload))` when the host handler reported an error, and
/// `Err(io::Error)` on a protocol fault. Value blobs are carried as raw bytes
/// — materialising one needs the run's isolate (see the poll loop in `v8.rs`).
///
/// Wire layout:
/// ```text
/// u32                 runId   (echoed from the BridgeCall being answered)
/// u32                 callId
/// u8                  ok
/// Optional<ValueBlob> value   (present when ok = true)
/// Optional<error>     error   (present when ok = false)
///   String code  String name  String message
///   Optional<String> stack  Optional<ValueBlob> fields
/// ```
#[allow(clippy::type_complexity)]
pub fn parse_bridge_response_payload(
    payload: &[u8],
) -> io::Result<(u32, u32, Result<Option<Vec<u8>>, BridgeErrorPayload>)> {
    let mut offset = 0;

    // runId — the host echoes the id from the BridgeCall it is answering, so
    // the session demux routes the frame statelessly. A stale response for a
    // finished run fails the demux lookup and is discarded.
    let run_id = read_u32(payload, &mut offset)?;

    // callId - returned to the caller for validation.  Within a run several
    // calls can be in flight (Promise.all), and the connection is reused
    // across runs, so a stale BridgeResponse from a previous run's orphaned
    // handler can arrive here.  The caller routes by this value.
    let call_id = read_u32(payload, &mut offset)?;

    let ok_byte = read_u8(payload, &mut offset)?;
    match ok_byte {
        1 => {
            // ok = true - read Optional<ValueBlob>
            let present = read_u8(payload, &mut offset)?;
            let value = if present == 1 {
                Some(read_value_blob(payload, &mut offset)?)
            } else {
                None
            };
            Ok((run_id, call_id, Ok(value)))
        }
        0 => {
            // ok = false - read the error payload: code name message stack fields
            let _code = read_string(payload, &mut offset)?;
            let name = read_string(payload, &mut offset)?;
            let message = read_string(payload, &mut offset)?;
            let stack_present = read_u8(payload, &mut offset)?;
            if stack_present == 1 {
                // Consume the stack string so the parser leaves positioned at
                // the fields value. The TS encoder never sends a host stack
                // (host internals must not leak into the sandbox), but the
                // wire slot exists for layout symmetry with RunErrorPayload.
                let _ = read_string(payload, &mut offset)?;
            }
            let fields_present = read_u8(payload, &mut offset)?;
            let fields = if fields_present == 1 {
                Some(read_value_blob(payload, &mut offset)?)
            } else {
                None
            };
            Ok((
                run_id,
                call_id,
                Err(BridgeErrorPayload {
                    name,
                    message,
                    fields,
                }),
            ))
        }
        b => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid BridgeResponse ok byte: {b:#04x}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::errors::MAX_ERROR_TEXT_BYTES;
    use crate::types::RunFailurePayload;

    fn failure_payload(message: String, stack: Option<String>, fields: Option<Vec<u8>>) -> Vec<u8> {
        encode_run_completion_payload(
            7,
            RunCompletion::Failure(RunFailurePayload {
                error: RunErrorPayload {
                    code: "ERR_USER_CODE".to_string(),
                    name: "Error".to_string(),
                    message,
                    stack,
                    fields,
                    reset: None,
                    prefix_stage: false,
                    cause: None,
                },
                stdout: Vec::new(),
                stderr: Vec::new(),
                duration_ms: 1.0,
                wall_time_ms: 1.0,
                cpu_time_ms: 1.0,
                bridge_calls: Vec::new(),
                heap_used_bytes: None,
            }),
        )
    }

    #[test]
    fn short_error_text_is_passed_through_unchanged() {
        assert_eq!(clamp_error_text("boom"), "boom");
    }

    #[test]
    fn oversized_error_text_is_truncated_on_a_char_boundary() {
        // 3-byte chars, so the cut offset is deliberately NOT a char boundary
        // and the backoff loop has to run. Slicing off-boundary would panic.
        let text = "€".repeat(MAX_ERROR_TEXT_BYTES);
        let clamped = clamp_error_text(&text);

        assert!(clamped.len() < text.len());
        assert!(clamped.len() <= MAX_ERROR_TEXT_BYTES + 64);
        assert!(clamped.ends_with(&format!("[truncated by iso4: {} bytes total]", text.len())));
    }

    #[test]
    fn a_huge_thrown_error_still_fits_in_one_frame() {
        // `throw Object.assign(new Error('A'.repeat(70e6)), { p: <100 MB> })`:
        // the shape that used to encode past the frame ceiling, fail to write,
        // and take the connection down with it — one dead pool slot per run.
        let payload = failure_payload(
            "A".repeat(70_000_000),
            Some("B".repeat(70_000_000)),
            Some(vec![0u8; 100_000_000]),
        );

        assert!(
            payload.len() < crate::ipc::DEFAULT_MAX_FRAME_LENGTH as usize,
            "failure payload was {} bytes, over the frame ceiling",
            payload.len()
        );
        // The caller must still learn what kind of failure it was.
        assert!(
            String::from_utf8_lossy(&payload).contains("ERR_USER_CODE"),
            "error code did not survive clamping"
        );
    }
}
