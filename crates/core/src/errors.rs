//! `RunError` and the failure output, the error codes, names and messages
//! per `docs/protocol.md` §7, and the cap on error text.

use std::borrow::Cow;

use crate::types;
use crate::types::{BridgeErrorPayload, ResetCause, ResetInfo, RunErrorPayload};
use crate::v8::isolate::{
    LogBuffers, ReasonCell, TerminationReason, WARMUP_CPU_MS, WARMUP_WALL_MS, elapsed_ms,
};

/// Ceiling on each piece of error text (message, stack): `max_export_bytes`
/// bounds only the success path, and an unbounded message once built a
/// Result frame past the 64 MiB frame ceiling.
pub const MAX_ERROR_TEXT_BYTES: usize = 64 * 1024;

/// Clamp error text to [`MAX_ERROR_TEXT_BYTES`], on a UTF-8 boundary, and say
/// so in the result rather than silently shortening it.
pub fn clamp_error_text(text: &str) -> Cow<'_, str> {
    if text.len() <= MAX_ERROR_TEXT_BYTES {
        return Cow::Borrowed(text);
    }
    let mut end = MAX_ERROR_TEXT_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    Cow::Owned(format!(
        "{}… [truncated by iso4: {} bytes total]",
        &text[..end],
        text.len()
    ))
}

/// The result of a failed JavaScript execution.
///
/// Logs are preserved on failures too: if user code logs and then throws, the
/// caller still receives the log output produced before the throw.
#[derive(Debug, Clone)]
pub struct FailureOutput {
    pub error: RunError,
    pub stdout: Vec<String>,
    pub stderr: Vec<String>,
    pub duration_ms: f64,

    /// Counted logic time — see `Output::wall_time_ms`.
    pub wall_time_ms: f64,

    /// Active V8 execution time — see `Output::cpu_time_ms`.
    pub cpu_time_ms: f64,

    /// Bridge call records — see `Output::bridge_calls`.
    pub bridge_calls: Vec<types::BridgeCallRecord>,
}

/// Payload carried by `RunError::RuntimeError`. Kept in a separate struct so
/// the variant can be boxed and the enum stays small (avoids
/// `clippy::result_large_err` on callers).
#[derive(Debug, Clone)]
pub struct RuntimeErrorData {
    pub name: String,
    pub message: String,
    pub stack: Option<String>,
    /// Own-enumerable properties of the thrown value beyond
    /// `name`/`message`/`stack`, as a V8 serialization blob holding a plain
    /// object. `None` when there are none.
    pub fields: Option<Vec<u8>>,
}

/// Map a codec refusal onto the run error the boundary reports.
///
/// `Unsupported` is a value the caller could have avoided sending;
/// `Malformed` is a corrupt payload, which is a protocol fault.
pub(crate) fn codec_error_to_run_error(error: crate::webcodec::CodecError) -> RunError {
    match error {
        crate::webcodec::CodecError::Unsupported(m) => RunError::TypeNotSerializable(m),
        crate::webcodec::CodecError::Malformed(m) => RunError::Internal(m),
    }
}

/// All the ways an execution can fail.
#[derive(Debug, Clone)]
pub enum RunError {
    /// Payload bytes are not valid UTF-8.
    /// For a frontend whose payload parsing fails — not yet wired up.
    #[allow(dead_code)]
    InvalidPayload(String),
    /// Uncaught exception thrown during execution.
    RuntimeError(Box<RuntimeErrorData>),
    /// No module with this specifier; the message names it.
    ModuleNotFound(String),
    /// Any other import failure: a missing named export, another V8
    /// instantiate failure, or a dynamic `import()` (which ends the run).
    Import(String),
    /// An export value is a function or an unresolved Promise.
    ExportNotSerializable(String),
    /// A registered host type (`Request`, `Response`, …) cannot cross this
    /// boundary in this position — an unimplemented tag, or content that is not
    /// self-contained such as a stream body. See `docs/protocol.md` §4.4.5.
    TypeNotSerializable(String),
    /// Active JS execution time exceeded `limits.cpuTimeMs`.
    CpuTimeout,
    /// Total wall-clock time exceeded `limits.wallTimeMs`.
    WallTimeout,
    /// V8 heap + ArrayBuffer exceeded `limits.memoryMb`.
    MemoryLimit,
    /// Configured host global/import handler threw or rejected and the
    /// sandbox did not catch it. Carries the handler error's `name`,
    /// `message`, and own-enumerable `fields` (never the host stack).
    HostBridge(Box<BridgeErrorPayload>),
    /// PrefixRun attempted to bind a global or import location that was
    /// never declared by Precompile.
    UndeclaredBinding(String),
    /// A host global (or bridge stub) uses a name the runtime owns
    /// ([`crate::v8::RESERVED_GLOBAL_NAMES`]). Shaped to also cover runtime-owned
    /// module specifiers if one ever becomes non-overridable.
    ReservedName(String),
    /// A PrefixRun rebind targets a declared location that is frozen with
    /// the prefix: a source module or a data leaf.
    FrozenBinding(String),
    /// Prefix top-level evaluation stayed pending after draining the
    /// microtask queue — it awaits something that can never resolve while a
    /// prefix evaluates (a never-settling promise).
    PrefixDidNotSettle(String),
    /// Prefix code called a bridge callable (bridge global, shim handler, or
    /// host-import function). No host session exists while a prefix
    /// evaluates, so the call can never be served.
    PrefixBridgeCall(String),
    /// Warm-up (isolate boot + prefix evaluation) blew its fixed budget
    /// ([`WARMUP_WALL_MS`]/[`WARMUP_CPU_MS`]). Not a per-request timeout —
    /// the caller's limits are not armed during warm-up, and the budget is
    /// deliberately not configurable (Cloudflare's script-startup model).
    WarmupLimit,
    /// The run needed a new isolate and the memory admission refused one:
    /// measured container usage + the run's own `memoryMb` would cross the
    /// hard line (or the run is uncapped while usage is at the budget).
    /// The message carries the measured numbers. Failed cleanly, never
    /// queued — the caller may retry when load falls.
    Capacity(String),
    /// A requested host → sandbox call's export path does not resolve against
    /// the module's exports, or resolves to something that is not callable
    ///. The message says which, and names the path.
    CallTargetNotFound(String),
    /// A function value was passed as a bridge argument.
    FunctionArgumentNotSupported,
    /// A bridge call payload (sandbox → host args) exceeded `limits.maxBridgeCallBytes`.
    BridgeCallPayloadTooLarge,
    /// Serialised exports blob exceeded `limits.maxExportBytes`.
    ExportTooLarge,
    /// Total bridge calls in this run exceeded `limits.maxBridgeCalls`.
    BridgeCallLimitExceeded,
    /// The host asked to stop the run via a `Terminate` frame (graceful
    /// abort). Surfaced to the sandbox consumer as `ERR_ABORTED`. Unlike the
    /// socket-teardown fallback, this arm carries the real duration, CPU time,
    /// and bridge-call records collected before the abort landed.
    Aborted,
    /// An innocent victim of a co-resident run's mid-JS interruption: a
    /// guard fired (CPU/memory) or a forced terminate landed while another
    /// run's turn was executing, so the shared instance can no longer be
    /// trusted and every in-flight run on it fails. Carries the cause class
    /// and the culprit's wire run id; the victim's own partial telemetry
    /// (duration, CPU time, bridge records) rides the failure payload as
    /// usual.
    InstanceReset {
        cause: ResetCause,
        culprit_run_id: u32,
    },
    /// Unexpected internal runtime failure.
    Internal(String),
}

impl RunError {
    /// The `ERR_*` code.
    pub fn code(&self) -> &'static str {
        match self {
            RunError::InvalidPayload(_) => "ERR_INTERNAL",
            RunError::RuntimeError(_) => "ERR_USER_CODE",
            RunError::ModuleNotFound(_) => "ERR_MODULE_NOT_FOUND",
            RunError::Import(_) => "ERR_IMPORT",
            RunError::ExportNotSerializable(_) => "ERR_EXPORT_NOT_SERIALIZABLE",
            RunError::TypeNotSerializable(_) => "ERR_TYPE_NOT_SERIALIZABLE",
            RunError::CpuTimeout => "ERR_CPU_TIMEOUT",
            RunError::WallTimeout => "ERR_WALL_TIMEOUT",
            RunError::MemoryLimit => "ERR_MEMORY_LIMIT",
            RunError::HostBridge(_) => "ERR_HOST_BRIDGE",
            RunError::UndeclaredBinding(_) => "ERR_UNDECLARED_BINDING",
            RunError::ReservedName(_) => "ERR_RESERVED_NAME",
            RunError::FrozenBinding(_) => "ERR_FROZEN_BINDING",
            RunError::PrefixDidNotSettle(_) => "ERR_PREFIX_DID_NOT_SETTLE",
            RunError::PrefixBridgeCall(_) => "ERR_PREFIX_BRIDGE_CALL",
            RunError::CallTargetNotFound(_) => "ERR_CALL_TARGET_NOT_FOUND",
            RunError::FunctionArgumentNotSupported => "ERR_FUNCTION_ARGUMENT_NOT_SUPPORTED",
            RunError::BridgeCallPayloadTooLarge => "ERR_BRIDGE_PAYLOAD_TOO_LARGE",
            RunError::ExportTooLarge => "ERR_EXPORT_TOO_LARGE",
            RunError::BridgeCallLimitExceeded => "ERR_BRIDGE_CALL_LIMIT_EXCEEDED",
            RunError::Aborted => "ERR_ABORTED",
            RunError::InstanceReset { .. } => "ERR_INSTANCE_RESET",
            RunError::WarmupLimit => "ERR_WARMUP_LIMIT",
            RunError::Capacity(_) => "ERR_CAPACITY_MEMORY",
            RunError::Internal(_) => "ERR_INTERNAL",
        }
    }

    /// The JS error name the host reports.
    pub fn name(&self) -> &str {
        match self {
            RunError::RuntimeError(inner) => &inner.name,
            RunError::HostBridge(err) => &err.name,
            RunError::TypeNotSerializable(_) => "TypeError",
            RunError::Aborted => "AbortError",
            _ => "Error",
        }
    }

    /// The human-readable message.
    pub fn message(&self) -> String {
        match self {
            RunError::InvalidPayload(msg)
            | RunError::ModuleNotFound(msg)
            | RunError::Import(msg)
            | RunError::ExportNotSerializable(msg)
            | RunError::TypeNotSerializable(msg)
            | RunError::UndeclaredBinding(msg)
            | RunError::ReservedName(msg)
            | RunError::FrozenBinding(msg)
            | RunError::PrefixDidNotSettle(msg)
            | RunError::PrefixBridgeCall(msg)
            | RunError::CallTargetNotFound(msg)
            | RunError::Capacity(msg)
            | RunError::Internal(msg) => msg.clone(),
            RunError::RuntimeError(inner) => inner.message.clone(),
            RunError::HostBridge(err) => err.message.clone(),
            RunError::CpuTimeout => "CPU time limit exceeded".to_string(),
            RunError::WallTimeout => "Wall time limit exceeded".to_string(),
            RunError::MemoryLimit => "Memory limit exceeded".to_string(),
            RunError::FunctionArgumentNotSupported => {
                "function arguments are not supported across the bridge boundary".to_string()
            }
            RunError::BridgeCallPayloadTooLarge => {
                "bridge call payload exceeds configured maxBridgeCallBytes limit".to_string()
            }
            RunError::ExportTooLarge => {
                "serialised export payload exceeds configured maxExportBytes limit".to_string()
            }
            RunError::BridgeCallLimitExceeded => {
                "run exceeded the configured maxBridgeCalls limit".to_string()
            }
            RunError::Aborted => "run was aborted".to_string(),
            RunError::InstanceReset { cause, .. } => format!(
                "the shared instance was reset by a co-resident run ({}): this run \
                 was in flight on it and could not continue",
                match cause {
                    ResetCause::Cpu => "CPU limit",
                    ResetCause::Memory => "memory limit",
                    ResetCause::Wall => "wall deadline",
                    ResetCause::Abort => "mid-execution abort",
                    ResetCause::Internal => "internal failure",
                    ResetCause::Unsupported => "unsupported operation",
                },
            ),
            RunError::WarmupLimit => format!(
                "prefix evaluation exceeded its fixed warm-up budget ({}ms wall / \
                 {}ms CPU); move expensive setup into the handler (lazy init on \
                 first call)",
                WARMUP_WALL_MS, WARMUP_CPU_MS,
            ),
        }
    }

    /// The structured error a completion payload carries. A runtime error's
    /// own stack and fields cross; a host error's stack never does (it may
    /// expose host paths and infrastructure details).
    pub fn to_payload(&self) -> RunErrorPayload {
        let (stack, fields) = match self {
            RunError::RuntimeError(inner) => (inner.stack.clone(), inner.fields.clone()),
            RunError::HostBridge(err) => (None, err.fields.clone()),
            _ => (None, None),
        };
        let reset = match self {
            RunError::InstanceReset {
                cause,
                culprit_run_id,
            } => Some(ResetInfo {
                cause: *cause,
                culprit_run_id: *culprit_run_id,
            }),
            _ => None,
        };
        RunErrorPayload {
            code: self.code().to_string(),
            name: self.name().to_string(),
            message: self.message(),
            stack,
            fields,
            reset,
            prefix_stage: false,
            cause: None,
        }
    }
}

/// The per-run failure for a bare mid-turn termination (a routed abort or
/// a guard fire observed by the caller, not by the turn itself).
pub(crate) fn reason_error(reason: &ReasonCell) -> RunError {
    match reason.get() {
        Some(TerminationReason::Wall) => RunError::WallTimeout,
        Some(TerminationReason::Cpu) => RunError::CpuTimeout,
        Some(TerminationReason::Memory) => RunError::MemoryLimit,
        Some(TerminationReason::Import(m)) => RunError::Import(m.to_string()),
        Some(TerminationReason::Abort) | None => RunError::Aborted,
    }
}

/// A guard can fire while the run is still in setup — installing globals or
/// the web runtime, evaluating the prefix, compiling a one-off module. V8 then
/// fails the interrupted operation, and classifying that failure would
/// misreport the timeout as `ERR_INTERNAL` ("evaluate web runtime: null") or
/// a `SyntaxError`. The guard stores its reason *before* it calls
/// `terminate_execution`, so a set reason here is always the real cause;
/// otherwise the fallback stands.
pub(crate) fn termination_or(reason: &ReasonCell, fallback: RunError) -> RunError {
    match reason.get() {
        Some(TerminationReason::Wall) => RunError::WallTimeout,
        Some(TerminationReason::Cpu) => RunError::CpuTimeout,
        Some(TerminationReason::Memory) => RunError::MemoryLimit,
        Some(TerminationReason::Abort) => RunError::Aborted,
        Some(TerminationReason::Import(m)) => RunError::Import(m.to_string()),
        None => fallback,
    }
}

pub(crate) fn failure(error: RunError, logs: &LogBuffers, start: std::time::Instant) -> FailureOutput {
    let duration_ms = elapsed_ms(start);
    FailureOutput {
        error,
        stdout: logs.stdout.clone(),
        stderr: logs.stderr.clone(),
        duration_ms,
        // Failures with run state get the real counted value stamped by
        // their concluder; setup failures ARE one own turn, so wall equals
        // the elapsed time.
        wall_time_ms: duration_ms,
        // Stamped by run_module from the shared run state (stays empty/0 for
        // precompile failures, which have no bridge and no CPU budget).
        cpu_time_ms: 0.0,
        bridge_calls: Vec::new(),
    }
}
