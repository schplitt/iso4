//! `RunError` codes, names and messages per `docs/protocol.md` §7, and the
//! cap on error text.

use std::borrow::Cow;

use crate::types::{ResetCause, ResetInfo, RunErrorPayload};
use crate::v8::{RunError, WARMUP_CPU_MS, WARMUP_WALL_MS};

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
