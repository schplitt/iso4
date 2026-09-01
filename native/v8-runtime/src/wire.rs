//! Frame payload encoders/decoders for the iso4 Rust ↔ TypeScript protocol.
//!
//! This module owns the **envelope**: run completion payloads, precompile
//! results, bridge call/response payloads, and the error mapping. It is
//! deliberately value-codec-free — every value slot is `u32 byteLength` + a V8
//! serialization blob (see `blob.rs` and `docs/protocol.md` §4), carried here
//! as opaque bytes because materialising one needs a V8 isolate.
//!
//! Design notes:
//! - All integers are big-endian.
//! - The strings in these payloads (`callId`, `exportName`, error `name` /
//!   `message`, …) are the control plane; blobs are the data plane.

use std::io;

use crate::v8::RunError;

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

// ── Completion payload types ───────────────────────────────────────────────────

/// Structured error fields for `RunCompletionPayload`.
pub struct RunErrorPayload {
    pub code: String,
    pub name: String,
    pub message: String,
    pub stack: Option<String>,
    /// Own-enumerable properties beyond `name`/`message`/`stack`, as a V8
    /// serialization blob holding a plain object. `None` when there are none.
    pub fields: Option<Vec<u8>>,
    /// Present only for `ERR_INSTANCE_RESET`: the cause class and the wire
    /// run id of the run whose mid-JS interruption reset the instance.
    pub reset: Option<ResetInfo>,
}

/// The `ERR_INSTANCE_RESET` extras — see `RunErrorPayload::reset`.
#[derive(Clone, Copy)]
pub struct ResetInfo {
    pub cause: crate::v8::ResetCause,
    pub culprit_run_id: u32,
}

fn reset_cause_byte(cause: crate::v8::ResetCause) -> u8 {
    match cause {
        crate::v8::ResetCause::Cpu => 0,
        crate::v8::ResetCause::Memory => 1,
        crate::v8::ResetCause::Abort => 2,
        crate::v8::ResetCause::Internal => 3,
        crate::v8::ResetCause::Wall => 4,
    }
}

/// Per-call metadata for one bridge call attempt — names, timing, and sizes
/// only, never payloads. Recorded by the runtime (`v8.rs`) and shipped on the
/// Result frame with the name already resolved — the runtime owns both the
/// import handle table and the shim naming convention, so no client-side
/// resolution remains.
#[derive(Debug, Clone)]
pub struct BridgeCallRecord {
    /// The public name sandbox code called: plain globals as-is (`fetch`),
    /// shimmed globals under their public name (not the private stub), and
    /// host-module import leaves as `<specifier>.<path>`.
    pub name: String,
    /// Offset from run start to the attempt, in ms (same clock as
    /// `duration_ms` on the run result).
    pub start_ms: f64,
    /// Round-trip time the sandbox waited (handler + IPC), in ms. For calls
    /// still unanswered when the run ended: time until run end. `0` for
    /// blocked attempts.
    pub duration_ms: f64,
    /// Serialized call payload size in bytes (the size `maxBridgeCallBytes`
    /// is enforced against). `0` for attempts blocked before serialisation.
    pub arg_bytes: u32,
    /// Serialized response value size in bytes. `0` on handler error or when
    /// the call never settled.
    pub response_bytes: u32,
    /// The host handler resolved and its response reached the sandbox.
    pub ok: bool,
    /// The attempt was blocked runtime-side (maxBridgeCalls, oversized
    /// payload, function argument) and never reached the host.
    pub blocked: bool,
}

/// Payload for a successful run.
pub struct RunSuccessPayload {
    /// All exports as **one** V8 serialization blob holding a plain
    /// `{ name: value }` object. The `default` export (if present) is the
    /// `"default"` key alongside named exports; an empty module produces a
    /// blob holding `{}`. For a run that carried a `call` this is the
    /// called function's return value instead — the host knows which it asked
    /// for, so the slot needs no tag.
    pub exports: Vec<u8>,
    /// Export names absent from `exports` because their value cannot cross
    /// the boundary. Always empty for a call run.
    pub skipped_exports: Vec<String>,
    pub stdout: Vec<String>,
    pub stderr: Vec<String>,
    pub duration_ms: f64,
    /// Active V8 execution time (bridge waits excluded), in ms.
    pub cpu_time_ms: f64,
    /// One record per bridge call attempt, in attempt order.
    pub bridge_calls: Vec<BridgeCallRecord>,
    /// `used_heap_size` of the isolate that served the run, measured after
    /// the run settled. Present for prefix runs (warm instances —
    /// feeds eviction scoring); absent for one-off runs, whose isolate
    /// is already gone.
    pub heap_used_bytes: Option<u64>,
    /// True when `waitUntil` background work is still running: a final
    /// `RunComplete` frame follows this Result on the same connection.
    pub background_pending: bool,
}

/// Payload for a failed run.
pub struct RunFailurePayload {
    pub error: RunErrorPayload,
    pub stdout: Vec<String>,
    pub stderr: Vec<String>,
    pub duration_ms: f64,
    /// Active V8 execution time (bridge waits excluded), in ms.
    pub cpu_time_ms: f64,
    /// One record per bridge call attempt, in attempt order.
    pub bridge_calls: Vec<BridgeCallRecord>,
    /// See `RunSuccessPayload::heap_used_bytes`.
    pub heap_used_bytes: Option<u64>,
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
///   ValueBlob  exports
///   List<String>  skippedExports
///   List<String>  stdout
///   List<String>  stderr
///   f64  durationMs
///   f64  cpuTimeMs
///   List<BridgeCallRecord>  bridgeCalls
///   Optional<u64>  heapUsedBytes   (present for prefix runs)
///   bool backgroundPending          (a RunComplete frame follows)
/// u8    failurePresent  (1 when ok = 0)
///   RunErrorPayload  error
///   List<String>  stdout
///   List<String>  stderr
///   f64  durationMs
///   f64  cpuTimeMs
///   List<BridgeCallRecord>  bridgeCalls
///   Optional<u64>  heapUsedBytes   (present for prefix runs)
/// ```
pub fn encode_run_completion_payload(run_id: u32, completion: RunCompletion) -> Vec<u8> {
    let mut out = Vec::new();
    encode_u32(run_id, &mut out);

    match completion {
        RunCompletion::Success(s) => {
            encode_bool(true, &mut out);
            out.push(1); // Optional<RunSuccessPayload> present
            encode_value_blob(&s.exports, &mut out);
            encode_string_list(&s.skipped_exports, &mut out);
            encode_string_list(&s.stdout, &mut out);
            encode_string_list(&s.stderr, &mut out);
            encode_f64(s.duration_ms, &mut out);
            encode_f64(s.cpu_time_ms, &mut out);
            encode_bridge_call_records(&s.bridge_calls, &mut out);
            encode_optional_u64(s.heap_used_bytes, &mut out);
            encode_bool(s.background_pending, &mut out);
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

/// Minimal `RunCompletePayload` substituted when the real grace report is too
/// large to frame (review, finding 1): same runId and status byte, empty
/// telemetry, and an error naming the substitution. Small by construction.
pub fn encode_minimal_run_complete(run_id: u32, status_byte: u8) -> Vec<u8> {
    let mut out = Vec::new();
    encode_u32(run_id, &mut out);
    out.push(status_byte);
    encode_f64(0.0, &mut out);
    encode_f64(0.0, &mut out);
    encode_string_list(&[], &mut out);
    encode_string_list(&[], &mut out);
    encode_bridge_call_records(&[], &mut out);
    out.push(1);
    encode_string("Error", &mut out);
    encode_string(
        "the grace report was too large to send; its telemetry was dropped",
        &mut out,
    );
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
/// bool          ok
/// bool          blocked
/// ```
fn encode_bridge_call_records(records: &[BridgeCallRecord], out: &mut Vec<u8>) {
    encode_u32(records.len() as u32, out);
    for r in records {
        encode_string(&r.name, out);
        encode_f64(r.start_ms, out);
        encode_f64(r.duration_ms, out);
        encode_u32(r.arg_bytes, out);
        encode_u32(r.response_bytes, out);
        encode_bool(r.ok, out);
        encode_bool(r.blocked, out);
    }
}

/// Ceiling on each piece of error *text* (message, stack) the runtime reports.
///
/// A thrown error's message and stack are copied verbatim out of the isolate,
/// and `max_export_bytes` bounds only the success path — so
/// `throw new Error('A'.repeat(70e6))` used to build a Result frame past the
/// 64 MiB frame ceiling, which could not be written and cost the connection.
/// 64 KiB is far past any real message while keeping the payload small.
const MAX_ERROR_TEXT_BYTES: usize = 64 * 1024;

/// Ceiling on the serialized own-properties blob attached to an error. Same
/// reasoning as [`MAX_ERROR_TEXT_BYTES`], but this one is a value blob and
/// cannot be truncated meaningfully, so an oversized one is dropped whole.
const MAX_ERROR_FIELDS_BYTES: usize = 1024 * 1024;

/// Clamp error text to [`MAX_ERROR_TEXT_BYTES`], on a UTF-8 boundary, and say
/// so in the result rather than silently shortening it.
fn clamp_error_text(text: &str) -> std::borrow::Cow<'_, str> {
    if text.len() <= MAX_ERROR_TEXT_BYTES {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut end = MAX_ERROR_TEXT_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    std::borrow::Cow::Owned(format!(
        "{}… [truncated by iso4: {} bytes total]",
        &text[..end],
        text.len()
    ))
}

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
    fn plain(code: &str, name: &str, message: String) -> RunErrorPayload {
        RunErrorPayload {
            code: code.to_string(),
            name: name.to_string(),
            message,
            stack: None,
            fields: None,
            reset: None,
        }
    }
    match error {
        RunError::InvalidPayload(msg) => plain("ERR_INTERNAL", "Error", msg.clone()),
        RunError::CompileError(msg) => plain("ERR_COMPILE", "SyntaxError", msg.clone()),
        RunError::RuntimeError(inner) => RunErrorPayload {
            code: "ERR_USER_CODE".to_string(),
            name: inner.name.clone(),
            message: inner.message.clone(),
            stack: inner.stack.clone(),
            fields: inner.fields.clone(),
            reset: None,
        },
        RunError::ModuleNotFound(msg) => plain("ERR_MODULE_NOT_FOUND", "Error", msg.clone()),
        RunError::ExportNotSerializable(msg) => {
            plain("ERR_EXPORT_NOT_SERIALIZABLE", "Error", msg.clone())
        }
        RunError::TypeNotSerializable(msg) => {
            plain("ERR_TYPE_NOT_SERIALIZABLE", "TypeError", msg.clone())
        }
        RunError::CpuTimeout => plain(
            "ERR_CPU_TIMEOUT",
            "Error",
            "CPU time limit exceeded".to_string(),
        ),
        RunError::WallTimeout => plain(
            "ERR_WALL_TIMEOUT",
            "Error",
            "Wall time limit exceeded".to_string(),
        ),
        RunError::MemoryLimit => plain(
            "ERR_MEMORY_LIMIT",
            "Error",
            "Memory limit exceeded".to_string(),
        ),
        RunError::HostBridge(err) => RunErrorPayload {
            code: "ERR_HOST_BRIDGE".to_string(),
            name: err.name.clone(),
            message: err.message.clone(),
            // Host stacks never cross the boundary — they may expose host
            // file paths and infrastructure details.
            stack: None,
            fields: err.fields.clone(),
            reset: None,
        },
        RunError::UndeclaredBinding(msg) => plain("ERR_UNDECLARED_BINDING", "Error", msg.clone()),
        RunError::PrefixDidNotSettle(msg) => {
            plain("ERR_PREFIX_DID_NOT_SETTLE", "Error", msg.clone())
        }
        RunError::PrefixBridgeCall(msg) => plain("ERR_PREFIX_BRIDGE_CALL", "Error", msg.clone()),
        RunError::CallTargetNotFound(msg) => {
            plain("ERR_CALL_TARGET_NOT_FOUND", "Error", msg.clone())
        }
        RunError::FunctionArgumentNotSupported => plain(
            "ERR_FUNCTION_ARGUMENT_NOT_SUPPORTED",
            "Error",
            "function arguments are not supported across the bridge boundary".to_string(),
        ),
        RunError::BridgeCallPayloadTooLarge => plain(
            "ERR_BRIDGE_PAYLOAD_TOO_LARGE",
            "Error",
            "bridge call payload exceeds configured maxBridgeCallBytes limit".to_string(),
        ),
        RunError::ExportTooLarge => plain(
            "ERR_EXPORT_TOO_LARGE",
            "Error",
            "serialised export payload exceeds configured maxExportBytes limit".to_string(),
        ),
        RunError::BridgeCallLimitExceeded => plain(
            "ERR_BRIDGE_CALL_LIMIT_EXCEEDED",
            "Error",
            "run exceeded the configured maxBridgeCalls limit".to_string(),
        ),
        RunError::Aborted => plain("ERR_ABORTED", "AbortError", "run was aborted".to_string()),
        RunError::InstanceReset {
            cause,
            culprit_run_id,
        } => RunErrorPayload {
            code: "ERR_INSTANCE_RESET".to_string(),
            name: "Error".to_string(),
            message: format!(
                "the shared instance was reset by a co-resident run ({}): this run \
                 was in flight on it and could not continue",
                match cause {
                    crate::v8::ResetCause::Cpu => "CPU limit",
                    crate::v8::ResetCause::Memory => "memory limit",
                    crate::v8::ResetCause::Wall => "wall deadline",
                    crate::v8::ResetCause::Abort => "mid-execution abort",
                    crate::v8::ResetCause::Internal => "internal failure",
                },
            ),
            stack: None,
            fields: None,
            reset: Some(ResetInfo {
                cause: *cause,
                culprit_run_id: *culprit_run_id,
            }),
        },
        RunError::WarmupLimit => plain(
            "ERR_WARMUP_LIMIT",
            "Error",
            format!(
                "prefix evaluation exceeded its fixed warm-up budget ({}ms wall / \
                 {}ms CPU); move expensive setup into the handler (lazy init on \
                 first call)",
                crate::v8::WARMUP_WALL_MS,
                crate::v8::WARMUP_CPU_MS,
            ),
        ),
        RunError::Internal(msg) => plain("ERR_INTERNAL", "Error", msg.clone()),
    }
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
    encode_count(stats.warm_idle, &mut out);
    encode_u64(stats.idle_heap_bytes, &mut out);
    encode_u64(stats.warm_budget_bytes, &mut out);
    encode_u64(stats.rss_bytes, &mut out);
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
/// u32                callId
/// u8                 targetKind  (0 = global, 1 = import)
/// Optional<String>   specifier   (present when targetKind = 1)
/// String             exportName
/// ValueBlob          args   (one blob holding the whole argument array)
/// ```
/// Everything in that layout except the argument bytes themselves, ending
/// with the blob's `u32` length prefix.
///
/// The arguments are already a serialized buffer by the time this runs, and
/// they are the only part of the payload with no bound: copying them into a
/// second buffer just to prepend this header doubles the peak memory of every
/// bridge call, and does it *before* `maxBridgeCallBytes` gets a chance to
/// refuse the call — so the cap could stop the send but never the allocation
/// it exists to prevent. The header's length plus the blob's is the exact
/// payload length, so the caller can check the cap first and then write the
/// two pieces in sequence.
pub fn encode_bridge_call_header(
    run_id: u32,
    call_id: u32,
    target_kind: u8, // 0 = global
    specifier: Option<&str>,
    export_name: &str,
    args_blob_len: u32,
) -> Vec<u8> {
    let mut out = Vec::new();
    // The owning run leads the frame, like every other run-scoped frame:
    // handlers are per run, and the host cannot map callId → run on its own
    // (call ids are allocated child-side). 0 = a direct-API run with no wire
    // identity.
    encode_u32(run_id, &mut out);
    encode_u32(call_id, &mut out);
    out.push(target_kind);
    match specifier {
        Some(s) => {
            out.push(1);
            encode_string(s, &mut out);
        }
        None => {
            out.push(0);
        }
    }
    encode_string(export_name, &mut out);
    encode_u32(args_blob_len, &mut out);
    out
}

// ── BridgeResponse payload decoder ───────────────────────────────────────────────

/// Error reported by a host bridge handler, as carried on a `BridgeResponse`
/// frame. Mirrors `RunErrorPayload` minus `code` (always `ERR_HOST_BRIDGE`)
/// and `stack` (the host stack is never exposed to sandbox code).
#[derive(Debug, Clone, PartialEq)]
pub struct BridgeErrorPayload {
    pub name: String,
    pub message: String,
    /// Own-enumerable properties of the host error, as a V8 serialization blob
    /// holding a plain object. `None` when the handler carried none.
    pub fields: Option<Vec<u8>>,
}

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
                },
                stdout: Vec::new(),
                stderr: Vec::new(),
                duration_ms: 1.0,
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
