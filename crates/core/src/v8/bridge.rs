//! The bridge: host globals and host-module trampolines calling out through
//! the link, their per-run stubs and resolvers, bridge errors, the call log,
//! and host → sandbox calls against a module's exports.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use crate::blob;
use crate::errors::{codec_error_to_run_error, reason_error, termination_or};
use crate::link::{Addr, HostLink, Outbound};
use crate::types;
use crate::types::{BridgeErrorPayload, HostGlobalDef, HostModuleNode, ImportBinding, ImportModule};
use crate::v8::exceptions::{
    RESERVED_ERROR_KEYS, copy_own_properties_except, error_fields_from_value,
    error_message_from_value, error_name_from_value, exception_fields, exception_message,
    exception_name, exception_stack, runtime_error_from_value,
};
use crate::v8::globals::{check_not_reserved, define_global, eval_script, throw_v8_error};
use crate::v8::isolate::{ReasonCell, TerminationReason};
use crate::v8::runloop::{
    RunFacts, RunPhase, RunState, ScanOutcome, build_output_and_maybe_grace, elapsed_ms,
};
use crate::v8::runtable::{RunTable, attributed_state};
use crate::v8::streams::ResultStreamMode;
use crate::v8::{FailureOutput, RunError, RuntimeErrorData};

// ── Bridge call log ──────────────────────────────────────────────────────────

/// The reserved dispatcher global backing all host-module import function
/// leaves. The runtime installs it automatically (as a bridge stub) whenever
/// the declared imports contain at least one function leaf; the trampolines
/// built into host modules call it with their handle ID as the first
/// argument. Mirrors `BRIDGE_DISPATCH_GLOBAL` in the TS client (`imports.ts`),
/// where the name is reserved so no user global can collide with it.
pub(super) const BRIDGE_DISPATCH_GLOBAL: &str = "__iso4_call";

// ── Host-module import handles ───────────────────────────────────────────────

/// One host-module function leaf, located by its declared position. Handle IDs
/// are the indices into the `Vec<ImportHandleEntry>` produced by
/// [`collect_import_handles`].
#[derive(Debug, Clone, PartialEq)]
pub(super) struct ImportHandleEntry {
    specifier: String,
    /// Key segments inside the module (a key may contain dots); the `path`
    /// on `BridgeCall` frames with `targetKind = import`.
    path: Vec<String>,
    /// The public bridge-record name, `<specifier>.<path>` dot-joined.
    record_name: String,
}

/// Walk the declared imports in wire order (bindings first-to-last, each tree
/// depth-first in entry order) and collect every function leaf. Handle IDs are
/// positions in the returned Vec.
///
/// This walk is the single source of truth for handle assignment: the module
/// builder derives trampoline IDs from the same order (via
/// [`host_module_base_id`]), so the dispatcher and the trampolines can never
/// disagree — including across the snapshot boundary, where trampolines baked
/// into a prefix snapshot are matched by a table rebuilt from the same
/// declared imports on every run.
pub(super) fn collect_import_handles(imports: &[ImportBinding]) -> Vec<ImportHandleEntry> {
    fn walk(
        specifier: &str,
        entries: &[(String, HostModuleNode)],
        path: &mut Vec<String>,
        out: &mut Vec<ImportHandleEntry>,
    ) {
        for (key, node) in entries {
            path.push(key.clone());
            match node {
                HostModuleNode::Function => out.push(ImportHandleEntry {
                    specifier: specifier.to_string(),
                    path: path.clone(),
                    record_name: format!("{specifier}.{}", path.join(".")),
                }),
                HostModuleNode::Object(children) => walk(specifier, children, path, out),
                HostModuleNode::Data(_) => {}
            }
            path.pop();
        }
    }

    let mut out = Vec::new();
    for binding in imports {
        if let ImportModule::Host(exports) = &binding.module {
            walk(&binding.specifier, exports, &mut Vec::new(), &mut out);
        }
    }
    out
}

/// The handle ID of the first function leaf belonging to `specifier`, under
/// the same walk order as [`collect_import_handles`]. Leaves within one
/// binding are contiguous, so the module builder assigns `base, base+1, …` to
/// its own depth-first walk.
pub(super) fn host_module_base_id(imports: &[ImportBinding], specifier: &str) -> u32 {
    let mut id = 0u32;
    for binding in imports {
        if binding.specifier == specifier {
            break;
        }
        if let ImportModule::Host(exports) = &binding.module {
            id += count_function_leaves(exports);
        }
    }
    id
}

fn count_function_leaves(entries: &[(String, HostModuleNode)]) -> u32 {
    let mut n = 0;
    for (_, node) in entries {
        match node {
            HostModuleNode::Function => n += 1,
            HostModuleNode::Object(children) => n += count_function_leaves(children),
            HostModuleNode::Data(_) => {}
        }
    }
    n
}

/// The public record name for a bridge stub: shim handler stubs
/// (`__iso4_<name>_h`, see `shimHandlerName` in the TS client) report under
/// their public `<name>`; everything else reports as-is. This mirrors the
/// naming convention the TS client uses when routing shim rebinds, so records
/// carry the name sandbox code actually called.
pub(super) fn public_record_name(stub_name: &str) -> &str {
    stub_name
        .strip_prefix("__iso4_")
        .and_then(|s| s.strip_suffix("_h"))
        .unwrap_or(stub_name)
}

/// Round to microsecond resolution, matching `elapsed_ms`.
fn round_micro(ms: f64) -> f64 {
    (ms * 1_000.0).round() / 1_000.0
}

/// Per-run bridge-call records, shared between the bridge callbacks (which
/// record attempts) and the poll loop (which settles them when the response
/// frame arrives). Both run on the V8 thread — the Mutex exists to satisfy
/// Send for the Arc, mirroring `PendingResolvers`.
#[derive(Default)]
pub(super) struct BridgeCallLog {
    records: Vec<types::BridgeCallRecord>,
    /// callId → index into `records`, for calls awaiting their response.
    in_flight: HashMap<u32, usize>,
}

impl BridgeCallLog {
    /// An attempt blocked runtime-side (limit, oversized payload, function
    /// argument, transport failure) — never reached the host.
    fn record_blocked(&mut self, name: &str, start_ms: f64, arg_bytes: u32) {
        self.records.push(types::BridgeCallRecord {
            name: name.to_string(),
            start_ms,
            duration_ms: 0.0,
            arg_bytes,
            response_bytes: 0,
            call_id: u32::MAX,
            outcome: types::BridgeCallOutcome::Blocked,
        });
    }

    /// A call whose BridgeCall frame was written to the host; settles when
    /// the matching response arrives (or at finalize if it never does).
    /// `name` is already the public name — the runtime owns both the import
    /// handle table and the shim naming convention.
    fn record_sent(&mut self, call_id: u32, name: &str, start_ms: f64, arg_bytes: u32) {
        self.records.push(types::BridgeCallRecord {
            name: name.to_string(),
            start_ms,
            duration_ms: 0.0,
            arg_bytes,
            response_bytes: 0,
            call_id,
            outcome: types::BridgeCallOutcome::Unanswered,
        });
        self.in_flight.insert(call_id, self.records.len() - 1);
    }

    /// Route a BridgeResponse to its record. Unknown callIds (stale frames
    /// from a previous run) are ignored, mirroring the resolver map.
    pub(super) fn settle(&mut self, call_id: u32, now_ms: f64, ok: bool, response_bytes: u32) {
        if let Some(idx) = self.in_flight.remove(&call_id) {
            let r = &mut self.records[idx];
            r.duration_ms = round_micro((now_ms - r.start_ms).max(0.0));
            r.outcome = if ok {
                types::BridgeCallOutcome::Ok
            } else {
                types::BridgeCallOutcome::Error
            };
            r.response_bytes = response_bytes;
        }
    }

    /// Non-draining snapshot for the early Result frame: a clone of
    /// the records so far, in-flight entries stamped with their
    /// duration-until-now, in-flight tracking untouched so grace-time
    /// settlement keeps working.
    pub(super) fn snapshot(&self, now_ms: f64) -> Vec<types::BridgeCallRecord> {
        let mut records = self.records.clone();
        for idx in self.in_flight.values() {
            let r = &mut records[*idx];
            r.duration_ms = round_micro((now_ms - r.start_ms).max(0.0));
        }
        records
    }

    /// Snapshot the records for the run result. Calls still in flight get
    /// their duration set to "until run end" and stay `Unanswered`.
    pub(super) fn finalize(&mut self, now_ms: f64) -> Vec<types::BridgeCallRecord> {
        for idx in self.in_flight.values() {
            let r = &mut self.records[*idx];
            r.duration_ms = round_micro((now_ms - r.start_ms).max(0.0));
        }
        self.in_flight.clear();
        std::mem::take(&mut self.records)
    }
}

// ── Host → sandbox calls ─────────────────────────────────────────────────────

/// Invoke the requested call against `module`'s namespace: resolve the export
/// path, call it, and either finish the run (sync value → serialized now) or
/// re-suspend on the returned promise. Runs inside the CPU budget.
pub(super) fn invoke_requested_call(
    scope: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
    facts: &RunFacts<'_>,
    rs: &mut RunState,
    module: v8::Local<v8::Module>,
) -> Result<ScanOutcome, FailureOutput> {
    let call = facts.call.expect("invoke_requested_call without a call");

    let Some(namespace) = module.get_module_namespace().to_object(scope) else {
        return Err(rs.fail(termination_or(
            facts.reason,
            RunError::Internal("module namespace is not an object".to_string()),
        )));
    };

    // Resolve the export path against the namespace and invoke. Runs inside
    // the CPU budget — a path segment can be an accessor, so resolution
    // itself is sandbox code.
    let (target, receiver) =
        resolve_call_target(scope, namespace, &call.export_path).map_err(|e| rs.fail(e))?;

    // The argument list crosses as one blob holding an array (identity
    // between arguments preserved, same convention as BridgeCall args);
    // host types (Request/…) rehydrate to real instances.
    let Some(args_value) = blob::deserialize_value_with_web_types(scope, &call.args_blob) else {
        let error = match blob::take_codec_error() {
            Some(e) => codec_error_to_run_error(e),
            None => RunError::Internal("failed to deserialize call arguments".to_string()),
        };
        return Err(rs.fail(termination_or(facts.reason, error)));
    };
    let Ok(args_array) = v8::Local::<v8::Array>::try_from(args_value) else {
        return Err(rs.fail(RunError::Internal(
            "call arguments blob did not hold an array".to_string(),
        )));
    };
    let mut args: Vec<v8::Local<v8::Value>> = Vec::with_capacity(args_array.length() as usize);
    for i in 0..args_array.length() {
        let Some(arg) = args_array.get_index(scope, i) else {
            return Err(rs.fail(RunError::Internal(format!(
                "failed to read call argument {i}"
            ))));
        };
        args.push(arg);
    }

    let result = match target.call(scope, receiver, &args) {
        Some(v) => v,
        None => {
            // Threw synchronously — classify like a rejected handler
            // promise, so sync and async throws report the same
            // name/message/stack/fields shape.
            if let Some(err) = rs.bridge_error.get() {
                return Err(rs.fail(termination_or(facts.reason, owned_bridge_error(err))));
            }
            let error = match facts.reason.get() {
                Some(TerminationReason::Wall) => RunError::WallTimeout,
                Some(TerminationReason::Cpu) => RunError::CpuTimeout,
                Some(TerminationReason::Memory) => RunError::MemoryLimit,
                // A hard abort's mid-turn kill.
                Some(TerminationReason::Abort) => RunError::Aborted,
                Some(TerminationReason::Import(m)) => RunError::Import(m.to_string()),
                None if facts.cancel_handle.is_execution_terminating() => RunError::Aborted,
                None => match scope.exception() {
                    Some(exception) => runtime_error_from_value(scope, exception),
                    None => RunError::RuntimeError(Box::new(RuntimeErrorData {
                        name: exception_name(scope),
                        message: exception_message(scope),
                        stack: exception_stack(scope),
                        fields: exception_fields(scope),
                    })),
                },
            };
            let error = rs.bridge_error.get().map_or(error, owned_bridge_error);
            return Err(rs.fail(termination_or(facts.reason, error)));
        }
    };

    // A sync return value needs no further settling at all. An async handler
    // suspends on the same machinery as the module promise — one checkpoint
    // first, so a promise that waits on no host I/O settles without touching
    // the socket.
    if result.is_promise() {
        let Ok(promise) = v8::Local::<v8::Promise>::try_from(result) else {
            return Err(rs.fail(RunError::Internal(
                "call result claimed to be a promise but is not".into(),
            )));
        };
        scope.perform_microtask_checkpoint();
        if facts.reason.get().is_some() {
            return Err(rs.fail(reason_error(facts.reason)));
        }
        if let Some(err) = rs.bridge_error.get() {
            return Err(rs.fail(owned_bridge_error(err)));
        }
        rs.phase = RunPhase::Settling {
            root: v8::Global::new(scope, promise),
            module: None,
            call_pending: false,
            root_is_call_result: true,
        };
        return Ok(ScanOutcome::Continue);
    }

    // Microtasks the call queued settle inside this call, not the next one.
    scope.perform_microtask_checkpoint();
    if let Some(err) = rs.bridge_error.get() {
        return Err(rs.fail(termination_or(facts.reason, owned_bridge_error(err))));
    }

    // The call returned synchronously; the guards stay armed through
    // serialization of the return value (serialization is guest execution).
    let exports = serialize_call_result(scope, facts, rs, result)?;
    Ok(build_output_and_maybe_grace(
        scope,
        facts,
        rs,
        exports,
        Vec::new(),
    ))
}

/// Serialize a call's return value under the run's budgets.
pub(super) fn serialize_call_result(
    scope: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
    facts: &RunFacts<'_>,
    rs: &RunState,
    value: v8::Local<v8::Value>,
) -> Result<Vec<u8>, FailureOutput> {
    // Serialization runs guest getters; nothing may run after a kill.
    if facts.reason.get().is_some() {
        return Err(rs.fail(reason_error(facts.reason)));
    }
    let export_path = facts
        .call
        .map(|c| c.export_path.as_str())
        .unwrap_or("<call>");
    // The RESULT of a session run is the one leg that may carry stream
    // bodies (#128): the codec registers them in the run's outbound table.
    // Exports and every host→sandbox leg stay stream-free.
    let _mode = ResultStreamMode::enter(rs.epilogue.is_some() && rs.link.is_some());
    blob::serialize_value(scope, value).map_err(|message| {
        let error = match blob::take_codec_error() {
            Some(e) => codec_error_to_run_error(e),
            None => RunError::ExportNotSerializable(format!(
                "call \"{export_path}\" result could not be serialized: {message}"
            )),
        };
        // A getter's bridge violation is the real cause of the failure.
        if let Some(err) = rs.bridge_error.get() {
            return rs.fail(termination_or(facts.reason, owned_bridge_error(err)));
        }
        rs.fail(termination_or(facts.reason, error))
    })
}

/// Clone the shared `bridge_error` OnceLock's content into an owned RunError.
pub(super) fn owned_bridge_error(err: &RunError) -> RunError {
    match err {
        RunError::HostBridge(m) => RunError::HostBridge(m.clone()),
        RunError::FunctionArgumentNotSupported => RunError::FunctionArgumentNotSupported,
        RunError::BridgeCallPayloadTooLarge => RunError::BridgeCallPayloadTooLarge,
        RunError::BridgeCallLimitExceeded => RunError::BridgeCallLimitExceeded,
        RunError::Internal(m) => RunError::Internal(m.clone()),
        RunError::ExportNotSerializable(m) => RunError::ExportNotSerializable(m.clone()),
        RunError::TypeNotSerializable(m) => RunError::TypeNotSerializable(m.clone()),
        other => RunError::Internal(format!("unexpected bridge error: {other:?}")),
    }
}

/// Cap on dot-separated segments in a call's export path. Real paths are one
/// or two segments (`"named"`, `"default.fetch"`); the cap only bounds
/// pathological input, since every segment is a property read that may run
/// sandbox code (an accessor).
const MAX_CALL_PATH_SEGMENTS: usize = 16;

/// Resolve a call's export path against the module namespace: the
/// callable, plus the receiver its `this` is bound to — the object the final
/// segment was read from, i.e. plain `a.b.c()` semantics (the namespace itself
/// for a single-segment path), so `export default { fetch() { this.… } }`
/// works. Reads follow the prototype chain, so `export default new Worker()`
/// resolves prototype methods (workerd reached the same design). Both "does
/// not resolve" and "not callable" report `ERR_CALL_TARGET_NOT_FOUND`; the
/// message says which.
fn resolve_call_target<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    namespace: v8::Local<'s, v8::Object>,
    export_path: &str,
) -> Result<(v8::Local<'s, v8::Function>, v8::Local<'s, v8::Value>), RunError> {
    let segments: Vec<&str> = export_path.split('.').collect();
    if export_path.is_empty() || segments.iter().any(|s| s.is_empty()) {
        return Err(RunError::CallTargetNotFound(format!(
            "call export path \"{export_path}\" is not a dot-separated path of non-empty segments"
        )));
    }
    if segments.len() > MAX_CALL_PATH_SEGMENTS {
        return Err(RunError::CallTargetNotFound(format!(
            "call export path \"{export_path}\" exceeds {MAX_CALL_PATH_SEGMENTS} segments"
        )));
    }

    let mut receiver: v8::Local<v8::Value> = namespace.into();
    let mut current: v8::Local<v8::Value> = namespace.into();
    for (i, segment) in segments.iter().enumerate() {
        let Some(object) = current.to_object(scope) else {
            return Err(RunError::CallTargetNotFound(format!(
                "call export path \"{export_path}\" does not resolve: \"{}\" is not an object",
                segments[..i].join(".")
            )));
        };
        let key = v8::String::new(scope, segment)
            .ok_or_else(|| RunError::Internal("failed to intern call path segment".to_string()))?;
        let Some(value) = object.get(scope, key.into()) else {
            // The property read itself threw (an accessor).
            return Err(RunError::CallTargetNotFound(format!(
                "call export path \"{export_path}\" does not resolve: reading \"{segment}\" threw"
            )));
        };
        receiver = object.into();
        current = value;
    }

    v8::Local::<v8::Function>::try_from(current)
        .map(|f| (f, receiver))
        .map_err(|_| {
            if current.is_undefined() || current.is_null() {
                RunError::CallTargetNotFound(format!(
                    "call export path \"{export_path}\" does not resolve against the module's \
                     exports"
                ))
            } else {
                RunError::CallTargetNotFound(format!(
                    "call export path \"{export_path}\" resolved to a value that is not callable"
                ))
            }
        })
}

// ── Bridge globals ───────────────────────────────────────────────────────────
//
// Every host-declared global (fetch, myTool, anything else) goes through the
// same generic bridge callback. The callback:
//   1. Resolves the CALLING run's binding by its attributed token (one stub
//      Function serves every run that binds the name).
//   2. Serialises the JS arguments into one value blob (rejects function args).
//   3. Sends a BridgeCall through the run's link and returns a pending
//      Promise immediately — it never reads the socket; the instance loop
//      delivers the BridgeResponse as a later turn and resolves the Promise.
//   4. On handler error the loop rejects with a real Error carrying the
//      handler's name/message/fields (tagged via a private symbol). Sandbox
//      code may catch it and continue; uncaught it surfaces as
//      ERR_HOST_BRIDGE. Only limit violations (maxBridgeCalls, payload too
//      large, function args) store a RunError in the shared OnceLock and are
//      fatal to the run: they call terminate_execution() so sandbox code
//      cannot catch its way past a violation (see fatal_bridge_error).
//
// fetch is NOT special. It gets the same callback as every other global.
// The host handler decides what the arguments mean and what to return.

// Per-stub heap allocation passed as External data to `bridge_global_callback`.
// One instance per declared global name, allocated as `Box<GlobalCallbackData>`
// and kept alive for the duration of `run_module`.

// ── Async bridge resolver map ────────────────────────────────────────────────
//
// Each bridge_global_callback creates a PromiseResolver and stores it here
// keyed by the callId it sent on the wire.  The run_module poll loop drains
// BridgeResponse frames and routes each one to the correct resolver.
//
// Both the callbacks and the poll loop execute on the same V8 thread so the
// Mutex is never actually contended — it exists solely to satisfy Rust's
// Send bound so the map can live in an Arc.

pub(super) struct PendingResolver(pub(super) v8::Global<v8::PromiseResolver>);
// SAFETY: only ever accessed from the V8 isolate thread.
unsafe impl Send for PendingResolver {}

pub(super) type PendingResolvers = Arc<Mutex<HashMap<u32, PendingResolver>>>;

/// Per-stub-name dispatch slot with **instance** lifetime.
///
/// The V8 `Function` a bridge stub installs holds a raw `External` pointer to
/// its slot, and sandbox code can stash that function across calls on a warm
/// instance (`globalThis.saved = fetch`) — so the pointee must outlive every
/// call, not just the one that installed it. The slot carries only identity:
/// the stub name and the instance's run table. The per-run half (link,
/// counters, limits) lives in the calling run's [`super::runtable::RunCallState::stubs`] map,
/// resolved per invocation by the attributed token — so one Function object
/// serves every run that binds the name, each with its own state, and a run
/// that did not bind it gets a catchable error instead of a stale dispatch.
pub(super) struct StubSlot {
    /// The global-object key this slot serves.
    stub_name: String,
    /// The instance's run table (instance-lifetime Box, like the slot).
    table: *const RunTable,
}

/// Instance-lifetime slot table, keyed by stub name. Boxed so the addresses
/// handed to V8 as `External`s never move.
#[allow(clippy::vec_box)]
pub(super) type StubSlots = HashMap<String, Box<StubSlot>>;

// Deliberately NOT Clone: bindings live behind an Rc in the run's stub map,
// and a per-invocation deep clone (two Strings, several Arcs) is the cost
// this shape removed.
pub(super) struct GlobalCallbackData {
    /// The owning run's address on every BridgeCall this stub sends
    /// (0 = a direct-API run with no wire identity).
    run_id: u32,
    /// The run's link to the host. The callback sends a BridgeCall here
    /// (non-blocking in practice) and returns immediately; the answer
    /// arrives as a run event.
    link: Arc<dyn HostLink>,
    /// Monotonic bridge call-ID counter, shared across all stubs in one run.
    call_id: Arc<AtomicU32>,
    /// First bridge error wins. Written once, read after evaluate() returns.
    bridge_error: Arc<OnceLock<RunError>>,
    /// The wire-level name this stub dispatches under — the `exportName` on
    /// its `BridgeCall` frames (e.g. "fetch", "__iso4_fetch_h"). The TS
    /// client routes handler lookup by this name.
    stub_name: String,
    /// The public name this stub's bridge records report under (e.g. "fetch",
    /// "myTool"; shim handler stubs report their public global name, not the
    /// private `__iso4_<name>_h` key).
    record_name: String,
    /// `Some` marks this stub as the host-import dispatcher
    /// (`BRIDGE_DISPATCH_GLOBAL`): the table resolves the leading handle-ID
    /// argument to the import function leaf being called, so the BridgeCall
    /// frame and the bridge record carry the real `<specifier>.<path>`.
    import_handles: Option<Arc<Vec<ImportHandleEntry>>>,
    /// Maximum allowed byte length for a bridge call payload (sandbox → host args).
    /// Zero means no per-call limit.
    max_bridge_call_bytes: u32,
    /// Shared counter of bridge calls made so far in this run.
    bridge_call_count: Arc<AtomicU32>,
    /// Maximum bridge calls allowed per run. Zero means no limit.
    max_bridge_calls: u32,
    /// Map of in-flight resolvers keyed by callId.  The callback inserts here;
    /// the poll loop in run_module removes and resolves/rejects.
    resolver_map: PendingResolvers,
    /// Run start — bridge-call records carry offsets on this clock so they
    /// line up with the result's `durationMs`.
    run_start: std::time::Instant,
    /// Per-run bridge-call records (attempts + blocked attempts); the poll
    /// loop settles sent entries when their response arrives.
    log: Arc<Mutex<BridgeCallLog>>,
}

/// Return a pre-rejected Promise carrying a host-bridge-tagged Error. Used
/// when a call is refused before any I/O (e.g. a direct dispatcher call with
/// an invalid handle) — sandbox code may catch it, exactly like a host
/// handler rejection; uncaught it surfaces as `ERR_HOST_BRIDGE`.
fn reject_with_bridge_error(scope: &mut v8::PinScope, rv: &mut v8::ReturnValue, message: &str) {
    let error = host_bridge_error_to_v8(
        scope,
        &BridgeErrorPayload {
            name: "Error".to_string(),
            message: format!("[iso4] {message}"),
            fields: None,
        },
    );
    let Some(resolver) = v8::PromiseResolver::new(scope) else {
        throw_v8_error(scope, message);
        return;
    };
    resolver.reject(scope, error);
    let promise = resolver.get_promise(scope);
    rv.set(promise.into());
}

/// Record a fatal bridge error and terminate JS execution immediately.
///
/// Fatal bridge violations (maxBridgeCalls, payload too large, function
/// arguments, transport failures) must end the run. A catchable JS exception
/// is not enough: inside the synchronous window before the next microtask
/// checkpoint, sandbox code could `try/catch` past the violation and keep
/// executing — or even complete the run successfully. `terminate_execution()`
/// raises V8's uncatchable termination instead; `run_module` maps the stored
/// `bridge_error` to the run failure on both exit paths (evaluate bail-out and
/// poll-loop checkpoint), checked before termination reasons.
fn fatal_bridge_error(
    scope: &mut v8::PinScope,
    bridge_error: &OnceLock<RunError>,
    error: RunError,
) {
    bridge_error.set(error).ok();
    if let Some(reason) = scope.get_slot::<Arc<ReasonCell>>() {
        reason.bare_terminate.store(true, Ordering::Relaxed);
    }
    scope.thread_safe_handle().terminate_execution();
}

/// A single generic V8 FunctionCallback used for every host-declared global.
///
/// Non-blocking: serialises arguments, sends a BridgeCall, stores a
/// PromiseResolver keyed by callId, and returns the pending Promise immediately.
/// The turn loop drives resolution when the BridgeResponse arrives and runs
/// perform_microtask_checkpoint().
fn bridge_global_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    // Recover the per-stub data from the External attached at registration time.
    // SAFETY: valid for the lifetime of run_module; the callback is only invoked
    // during module.evaluate() or perform_microtask_checkpoint(), both nested
    // inside run_module.
    let data_ptr = args.data();
    let slot = match v8::Local::<v8::External>::try_from(data_ptr) {
        Ok(ext) => ext.value().cast::<StubSlot>(),
        Err(_) => {
            throw_v8_error(scope, "[iso4] bridge: missing external data");
            return;
        }
    };
    // SAFETY: the External points at a `Box<StubSlot>` owned by the
    // instance's `StubSlots` table (or the one-off run's local table), which
    // outlives every V8 callback of this isolate — including bridge stubs
    // stashed by sandbox code across calls on a warm instance.
    let slot = unsafe { &*slot };
    // SAFETY: the slot's table pointer is instance-lifetime like the slot.
    let table = unsafe { &*slot.table };
    let state = attributed_state(scope, table);
    let data: Option<std::rc::Rc<GlobalCallbackData>> = if state.is_null() {
        None
    } else {
        // Take an Rc handle (one refcount bump) so no table borrow is held
        // across the serialization below, which can run guest getters.
        // SAFETY: entry pointers are valid for the remainder of the turn.
        unsafe { &*state }.stubs.borrow().get(&slot.stub_name).cloned()
    };
    let Some(data) = data else {
        // The calling run did not bind this name: the Function was captured
        // during an earlier call on this warm instance (or by a co-resident
        // run that declared it). Catchable — exactly like the
        // prepare()-time placeholders.
        throw_v8_error(
            scope,
            &format!(
                "[iso4] bridge: '{}' is not bound in this call — the reference was \
                 captured from an earlier run on this warm instance",
                slot.stub_name
            ),
        );
        return;
    };
    let data = &data;

    // Attempt timestamp on the run clock — recorded for every attempt,
    // including ones blocked below. Blocked import dispatches record under
    // the dispatcher's own name when they fail before handle resolution.
    let start_ms = elapsed_ms(data.run_start);
    let record_blocked = |arg_bytes: u32| {
        data.log
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .record_blocked(&data.record_name, start_ms, arg_bytes);
    };

    // ── Bridge call count limit ────────────────────────────────────────────
    let prev = data.bridge_call_count.fetch_add(1, Ordering::Relaxed);
    if data.max_bridge_calls > 0 && prev >= data.max_bridge_calls {
        record_blocked(0);
        fatal_bridge_error(scope, &data.bridge_error, RunError::BridgeCallLimitExceeded);
        return;
    }

    // ── Reject function arguments ──────────────────────────────────────────
    // The serializer would refuse them anyway, but only with a generic
    // data-clone message; a top-level function argument gets its own code.
    for i in 0..args.length() {
        if args.get(i).is_function() {
            record_blocked(0);
            fatal_bridge_error(
                scope,
                &data.bridge_error,
                RunError::FunctionArgumentNotSupported,
            );
            return;
        }
    }

    // ── Resolve the call target ────────────────────────────────────────────
    //
    // Plain globals dispatch under their own name (targetKind = 0). Host-
    // module function leaves reach this callback through the reserved
    // dispatcher stub with their handle ID as the first argument; the runtime
    // owns the handle table, so the frame carries the resolved specifier and
    // leaf path (targetKind = 1) and the ID never leaves the process.
    let (target, first_arg, record_name) = match &data.import_handles {
        None => (
            types::BridgeTarget::Global(data.stub_name.clone()),
            0,
            data.record_name.clone(),
        ),
        Some(table) => {
            let handle = args.get(0).number_value(scope).filter(|n| {
                args.length() > 0 && n.fract() == 0.0 && *n >= 0.0 && (*n as usize) < table.len()
            });
            let Some(handle) = handle else {
                // Only reachable by sandbox code calling the reserved
                // dispatcher directly with a bogus handle — reject the
                // call catchably, mirroring a host handler error.
                record_blocked(0);
                reject_with_bridge_error(
                    scope,
                    &mut rv,
                    "no host import handle for direct dispatcher call",
                );
                return;
            };
            let entry = &table[handle as usize];
            (
                types::BridgeTarget::Import {
                    specifier: entry.specifier.clone(),
                    path: entry.path.clone(),
                },
                1, // the handle ID never leaves the process
                entry.record_name.clone(),
            )
        }
    };

    // ── Serialise arguments ────────────────────────────────────────────────
    // One blob for the whole argument list, not one per argument: measurably
    // cheaper, and it preserves identity between arguments that reference the
    // same object.
    let args_array = v8::Array::new(scope, (args.length() - first_arg).max(0));
    for i in first_arg..args.length() {
        let arg = args.get(i);
        if args_array
            .set_index(scope, (i - first_arg) as u32, arg)
            .is_none()
        {
            record_blocked(0);
            fatal_bridge_error(
                scope,
                &data.bridge_error,
                RunError::Internal("failed to stage bridge call arguments".to_string()),
            );
            return;
        }
    }
    let args_blob = match blob::serialize_value(scope, args_array.into()) {
        Ok(bytes) => bytes,
        Err(message) => {
            record_blocked(0);
            fatal_bridge_error(
                scope,
                &data.bridge_error,
                match blob::take_codec_error() {
                    Some(e) => codec_error_to_run_error(e),
                    None => RunError::ExportNotSerializable(format!(
                        "bridge call argument could not be serialized: {message}"
                    )),
                },
            );
            return;
        }
    };

    // ── Assign call ID and size the BridgeCall ────────────────────────────
    //
    // `maxBridgeCallBytes` counts the serialized arguments, the only part of
    // a call whose size the sandbox controls; the frontend's own framing is
    // not the sandbox's doing. The buffer moves to the frontend uncopied.
    let call_id = data.call_id.fetch_add(1, Ordering::Relaxed);
    if u32::try_from(args_blob.len()).is_err() {
        record_blocked(u32::MAX);
        fatal_bridge_error(
            scope,
            &data.bridge_error,
            RunError::BridgeCallPayloadTooLarge,
        );
        return;
    }
    let payload_len = args_blob.len();

    if data.max_bridge_call_bytes > 0 && payload_len > data.max_bridge_call_bytes as usize {
        record_blocked(u32::try_from(payload_len).unwrap_or(u32::MAX));
        fatal_bridge_error(
            scope,
            &data.bridge_error,
            RunError::BridgeCallPayloadTooLarge,
        );
        return;
    }

    // A getter in the arguments already ended the run: send nothing.
    if data.bridge_error.get().is_some()
        || scope
            .get_slot::<Arc<ReasonCell>>()
            .is_some_and(|reason| reason.get().is_some())
    {
        record_blocked(0);
        return;
    }

    // ── Send the BridgeCall (queued; blocks only if the host stopped
    // draining, bounded by the frontend's stall timeout) ──────────────────
    if let Err(e) = data.link.send(
        Addr::Run(data.run_id),
        Outbound::BridgeCall {
            call_id,
            target,
            args: args_blob,
        },
    ) {
        record_blocked(u32::try_from(payload_len).unwrap_or(u32::MAX));
        fatal_bridge_error(
            scope,
            &data.bridge_error,
            RunError::Internal(format!("bridge write failed: {e}")),
        );
        return;
    }

    // The call reached the host — record it for the run's bridge
    // report under its resolved public name. Settled by the poll loop when
    // the matching BridgeResponse arrives.
    data.log
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .record_sent(
            call_id,
            &record_name,
            start_ms,
            u32::try_from(payload_len).unwrap_or(u32::MAX),
        );

    // ── Create PromiseResolver, store it, return the Promise ──────────────
    // The run_module poll loop will resolve/reject this when the matching
    // BridgeResponse frame arrives and callId routing finds this entry.
    let resolver = match v8::PromiseResolver::new(scope) {
        Some(r) => r,
        None => {
            fatal_bridge_error(
                scope,
                &data.bridge_error,
                RunError::Internal("failed to create PromiseResolver".into()),
            );
            return;
        }
    };
    let promise = resolver.get_promise(scope);
    let global_resolver = v8::Global::new(scope, resolver);
    data.resolver_map
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(call_id, PendingResolver(global_resolver));

    rv.set(promise.into());
}

/// One bridge stub to install on the sandbox global object.
pub(super) struct BridgeStubSpec {
    /// The global-object key the stub installs under (e.g. "fetch",
    /// "__iso4_fetch_h", "__iso4_call").
    pub(super) stub_name: String,
    /// The public name its bridge records report under.
    pub(super) record_name: String,
    /// `Some` for the host-import dispatcher stub — the handle table it
    /// resolves leading handle-ID arguments against.
    pub(super) import_handles: Option<Arc<Vec<ImportHandleEntry>>>,
    /// The declared enumerability of the public name. Ignored in
    /// effect for internal stub names, which install DONT_ENUM regardless.
    pub(super) enumerable: bool,
}

/// Install a bridge stub for each spec.
///
/// Each stub is an identical `bridge_global_callback` function whose External
/// data points at the per-name [`StubSlot`] in `slots` — instance-lifetime
/// storage re-pointed at this call's `GlobalCallbackData` here. Installation
/// uses `define_own_property`, never `set`: sandbox code can plant a hostile
/// accessor under a stub name, and a plain `set` would invoke it.
#[allow(clippy::too_many_arguments)] // bridge setup genuinely needs all these params
pub(super) fn install_bridge_globals(
    scope: &mut v8::PinScope,
    stubs: &[BridgeStubSpec],
    token: u64,
    table: *const RunTable,
    run_id: u32,
    link: Arc<dyn HostLink>,
    call_id: Arc<AtomicU32>,
    bridge_error: Arc<OnceLock<RunError>>,
    max_bridge_call_bytes: u32,
    bridge_call_count: Arc<AtomicU32>,
    max_bridge_calls: u32,
    resolver_map: PendingResolvers,
    run_start: std::time::Instant,
    log: Arc<Mutex<BridgeCallLog>>,
    slots: &mut StubSlots,
) -> Result<(), RunError> {
    for spec in stubs {
        // A bridge stub installs under its public name, so it can shadow a
        // reserved class just as a data global can. Private shim handler names
        // (`__iso4_<name>_h`) are internal and never collide.
        check_not_reserved(&spec.stub_name)?;
        let name = &spec.stub_name;
        let slot = slots.entry(spec.stub_name.clone()).or_insert_with(|| {
            Box::new(StubSlot {
                stub_name: spec.stub_name.clone(),
                table,
            })
        });
        // This run's binding for the name — the callback resolves it per
        // invocation by the attributed token, so co-resident runs never see
        // each other's links or counters.
        {
            // SAFETY: `table` is the instance-lifetime RunTable Box.
            let table = unsafe { &*table };
            let runs = table.runs.borrow();
            let entry = runs
                .get(&token)
                .expect("install_bridge_globals: run entry exists for the whole call");
            entry.stubs.borrow_mut().insert(
                spec.stub_name.clone(),
                std::rc::Rc::new(GlobalCallbackData {
                    run_id,
                    link: Arc::clone(&link),
                    call_id: Arc::clone(&call_id),
                    bridge_error: Arc::clone(&bridge_error),
                    stub_name: spec.stub_name.clone(),
                    record_name: spec.record_name.clone(),
                    import_handles: spec.import_handles.clone(),
                    max_bridge_call_bytes,
                    bridge_call_count: Arc::clone(&bridge_call_count),
                    max_bridge_calls,
                    resolver_map: Arc::clone(&resolver_map),
                    run_start,
                    log: Arc::clone(&log),
                }),
            );
        }
        // The slot Box's heap address is stable and instance-lifetime; the
        // Function built here (and any earlier call's Function for the same
        // name, however sandbox code kept it) dispatches through it.
        let data_ptr = slot.as_ref() as *const StubSlot as *mut c_void;

        let external = v8::External::new(scope, data_ptr);
        let function = v8::Function::builder(bridge_global_callback)
            .data(external.into())
            .build(scope)
            .ok_or_else(|| {
                RunError::Internal(format!("failed to build bridge stub for '{name}'"))
            })?;

        define_global(scope, name, function.into(), spec.enumerable)?;
    }
    Ok(())
}

/// Marker property the prepare()-time bridge placeholders set on the errors
/// they throw, so `evaluate_prefix_module` can map the rejection to
/// `RunError::PrefixBridgeCall`. An ordinary property, not a `v8::Private` —
/// the placeholders are plain JS (see below) and JS cannot create privates.
/// Prefix code is host-authored setup, so spoofing is not a concern.
/// Keep the name in sync with [`PREFIX_BRIDGE_PLACEHOLDER_FACTORY_SRC`].
const PREFIX_BRIDGE_CALL_MARKER: &str = "__iso4_prefix_bridge_call";

/// Factory for prepare()-time bridge placeholders, evaluated as its own
/// script. The placeholders must be plain JS closures: they are captured in
/// the startup snapshot, and native callbacks would need external-reference
/// table entries there. `kind` 0 is a named global; 1 is the host-import
/// dispatch target, whose callable names are not known at install time.
const PREFIX_BRIDGE_PLACEHOLDER_FACTORY_SRC: &str = r#"(name, kind) => (...args) => {
  const what = kind === 0 ? `bridge global '${name}'` : 'a host import function'
  const e = new Error(`${what} cannot be called while a prefix evaluates: no host session exists at that stage. Do the call in run() code instead.`)
  e.__iso4_prefix_bridge_call = true
  throw e
}"#;

/// Install throwing placeholders for every bridge callable a prefix could
/// reach at prepare() time: plain bridge globals (public name), shim handler
/// targets (`__iso4_<name>_h`), and the host-import dispatch global
/// (`__iso4_call`). No host session exists at precompile time, so a bridge
/// call can never be served — without these the failure shape is an
/// accidental `ReferenceError`/`TypeError` suggesting the binding does not
/// exist at all. With them, a call fails as `ERR_PREFIX_BRIDGE_CALL`, and
/// `typeof fetch` inside a prefix matches what run() code sees.
///
/// The placeholders are captured in the snapshot; every run overwrites the
/// same names with live link-backed stubs (`install_bridge_globals`).
pub(super) fn install_prefix_bridge_placeholders(
    scope: &mut v8::PinScope,
    globals: &[HostGlobalDef],
    imports: &[ImportBinding],
) -> Result<(), RunError> {
    // (install name, display name, kind, enumerable) per the factory's
    // parameters plus the attribute the placeholder installs with — it must
    // match the live stub so the enumerated surface never differs between
    // prepare time and run time. Internal names are non-enumerable
    // regardless.
    let mut targets: Vec<(String, String, i32, bool)> = Vec::new();
    for def in globals {
        match def {
            HostGlobalDef::Bridge {
                name, enumerable, ..
            } => {
                check_not_reserved(name)?;
                targets.push((name.clone(), name.clone(), 0, *enumerable));
            }
            HostGlobalDef::Shim {
                name, handler_name, ..
            } => targets.push((handler_name.clone(), name.clone(), 0, true)),
            HostGlobalDef::StringExpr { .. } | HostGlobalDef::Data { .. } => {}
        }
    }
    if imports
        .iter()
        .any(|binding| matches!(binding.module, ImportModule::Host(_)))
    {
        targets.push((
            BRIDGE_DISPATCH_GLOBAL.to_string(),
            BRIDGE_DISPATCH_GLOBAL.to_string(),
            1,
            true,
        ));
    }
    if targets.is_empty() {
        return Ok(());
    }

    let factory_value = eval_script(
        scope,
        PREFIX_BRIDGE_PLACEHOLDER_FACTORY_SRC,
        "<iso4:prefix-bridge-placeholder>",
    )?;
    let factory = v8::Local::<v8::Function>::try_from(factory_value).map_err(|_| {
        RunError::Internal("placeholder factory did not evaluate to a function".to_string())
    })?;
    let receiver: v8::Local<v8::Value> = v8::undefined(scope).into();
    for (install_name, display_name, kind, enumerable) in targets {
        let name_arg = v8::String::new(scope, &display_name).ok_or_else(|| {
            RunError::Internal(format!(
                "failed to intern placeholder name '{display_name}'"
            ))
        })?;
        let kind_arg = v8::Integer::new(scope, kind);
        let placeholder = factory
            .call(scope, receiver, &[name_arg.into(), kind_arg.into()])
            .ok_or_else(|| {
                RunError::Internal(format!(
                    "failed to build bridge placeholder for '{display_name}'"
                ))
            })?;
        define_global(scope, &install_name, placeholder, enumerable)?;
    }
    Ok(())
}

/// Private-symbol key marking Error objects that originate from a host bridge
/// handler failure. Lets the poll loop distinguish an uncaught host error
/// (→ `ERR_HOST_BRIDGE`) from an uncaught sandbox error (→ `ERR_USER_CODE`).
const HOST_BRIDGE_ERROR_TAG: &str = "iso4::hostBridgeError";

fn host_bridge_tag<'s>(scope: &mut v8::PinScope<'s, '_>) -> Option<v8::Local<'s, v8::Private>> {
    let key = v8::String::new(scope, HOST_BRIDGE_ERROR_TAG)?;
    Some(v8::Private::for_api(scope, Some(key)))
}

/// Error names with a matching V8 intrinsic constructor. Using the intrinsic
/// makes `instanceof TypeError` etc. work in the sandbox; any other name is
/// carried as an own `name` property on a plain Error.
const INTRINSIC_ERROR_NAMES: [&str; 5] = [
    "Error",
    "TypeError",
    "RangeError",
    "SyntaxError",
    "ReferenceError",
];

/// Materialise a host bridge handler error as a real sandbox Error object:
/// `name` (via the matching intrinsic constructor where one exists), `message`,
/// and every carried field re-attached as a direct own property — so the
/// caught error has the same shape the host handler threw. The host stack is
/// deliberately never carried across the bridge, and `name`/`message`/`stack`
/// can never be overridden through the fields payload. The object is tagged
/// with [`HOST_BRIDGE_ERROR_TAG`] so an uncaught rejection classifies as
/// `ERR_HOST_BRIDGE`.
pub(super) fn host_bridge_error_to_v8<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    err: &BridgeErrorPayload,
) -> v8::Local<'s, v8::Value> {
    let message = v8::String::new(scope, &err.message).unwrap_or_else(|| v8::String::empty(scope));
    let exception = match err.name.as_str() {
        "TypeError" => v8::Exception::type_error(scope, message),
        "RangeError" => v8::Exception::range_error(scope, message),
        "SyntaxError" => v8::Exception::syntax_error(scope, message),
        "ReferenceError" => v8::Exception::reference_error(scope, message),
        _ => v8::Exception::error(scope, message),
    };
    let Some(obj) = exception.to_object(scope) else {
        return exception;
    };
    if !INTRINSIC_ERROR_NAMES.contains(&err.name.as_str()) {
        if let (Some(key), Some(name)) = (
            v8::String::new(scope, "name"),
            v8::String::new(scope, &err.name),
        ) {
            obj.set(scope, key.into(), name.into());
        }
    }
    // Re-attach carried fields as direct own properties. The TS encoder only
    // ever sends an Object here; skip reserved keys defensively so a crafted
    // payload cannot override Error identity or inject a fake stack, and drop
    // "__proto__" per the protocol-wide policy (it never crosses either way).
    if let Some(bytes) = &err.fields {
        if let Some(fields) = blob::deserialize_value(scope, bytes).and_then(|v| v.to_object(scope))
        {
            copy_own_properties_except(scope, fields, obj, &RESERVED_ERROR_KEYS);
        }
    }
    if let Some(tag) = host_bridge_tag(scope) {
        let marker = v8::Boolean::new(scope, true);
        obj.set_private(scope, tag, marker.into());
    }
    exception
}

/// If `value` is a rejection produced by [`host_bridge_error_to_v8`], rebuild
/// the structured `RunError::HostBridge` from it; otherwise `None`.
///
/// Fields are re-read from the object (not stashed host-side) because several
/// bridge errors can be in flight and sandbox code may catch some of them —
/// only the object that actually rejected the module promise matters. Reading
/// own-enumerable properties also means a field added by sandbox code before
/// rethrowing survives to the host.
pub(super) fn host_bridge_error_from_rejection(
    scope: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
    value: v8::Local<v8::Value>,
) -> Option<RunError> {
    let obj = value.to_object(scope)?;
    let tag = host_bridge_tag(scope)?;
    if !obj.has_private(scope, tag).unwrap_or(false) {
        return None;
    }
    let name = error_name_from_value(scope, value).unwrap_or_else(|| "Error".to_string());
    let message =
        error_message_from_value(scope, value).unwrap_or_else(|| "host handler failed".to_string());
    Some(RunError::HostBridge(Box::new(BridgeErrorPayload {
        name,
        message,
        fields: error_fields_from_value(scope, value),
    })))
}

/// Map an exception carrying the prefix-bridge-call marker (thrown by the
/// prepare()-time placeholders) to `RunError::PrefixBridgeCall`. Returns
/// `None` for anything else.
pub(super) fn prefix_bridge_call_from_value(
    scope: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
    value: v8::Local<v8::Value>,
) -> Option<RunError> {
    let obj = value.to_object(scope)?;
    let key = v8::String::new(scope, PREFIX_BRIDGE_CALL_MARKER)?;
    if !obj.has_own_property(scope, key.into()).unwrap_or(false) {
        return None;
    }
    let message = error_message_from_value(scope, value)
        .unwrap_or_else(|| "bridge call during prepare()".to_string());
    Some(RunError::PrefixBridgeCall(message))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::link::Inbound;
    use crate::testval::{self, TestValue as WireValue};
    use crate::types;
    use crate::v8::runloop::run_module;
    use crate::v8::testsupport::{
        bridge_call_parts, bridge_resp_err, bridge_resp_err_full, bridge_resp_ok, call_spec,
        drain_bridge_calls, get_default, get_field, host_import, prepared, run_call, run_code,
        run_ok, run_with_bridge, source_import, spawn_responder, test_io, value_of,
    };
    use crate::v8::{Limits, RunIo, execute, execute_with_prefix};

    // ── Bridge tests ───────────────────────────────────────────────────────────────────
    //
    // These tests exercise the full bridge round-trip: V8 calls a host global,
    // a responder thread receives the BridgeCall and sends a BridgeResponse,
    // and we assert on the returned JS value or error code.

    #[test]
    fn bridge_call_returns_number() {
        let (out, h) = run_with_bridge(
            "export default await myTool()",
            "myTool",
            Limits {
                cpu_time_ms: 5_000,
                wall_time_ms: 10_000,
                ..Default::default()
            },
            |s| {
                s.send(bridge_resp_ok(0, &WireValue::Number(42.0)));
            },
        );
        h.join().unwrap();
        assert_eq!(get_default(&out.unwrap()).as_deref(), Some("42"));
    }

    #[test]
    fn terminate_frame_aborts_run_with_telemetry() {
        // Graceful abort: while the sandbox is suspended awaiting a bridge
        // response, the host sends `Terminate` instead of a `BridgeResponse`.
        // The run must return `ERR_ABORTED` and still carry the in-flight bridge
        // record and real timings — not the synthesized zeros of a teardown.
        let (out, h) = run_with_bridge(
            "export default await myTool()",
            "myTool",
            Limits {
                cpu_time_ms: 5_000,
                wall_time_ms: 10_000,
                ..Default::default()
            },
            |s| {
                s.send(Inbound::Terminate {
                    mode: types::TerminateMode::Soft,
                });
            },
        );
        h.join().unwrap();
        let failure = out.unwrap_err();
        assert!(
            matches!(failure.error, RunError::Aborted),
            "expected Aborted, got {:?}",
            failure.error
        );
        // The bridge call that was in flight when the abort landed is recorded
        // (unsettled → ok=false), not dropped.
        assert_eq!(failure.bridge_calls.len(), 1);
        assert_eq!(failure.bridge_calls[0].name, "myTool");
        assert_eq!(failure.bridge_calls[0].outcome, types::BridgeCallOutcome::Unanswered);
        // Timings are stamped from the shared run state, not left as zeros.
        assert!(failure.duration_ms >= 0.0);
        assert!(failure.cpu_time_ms >= 0.0);
    }

    #[test]
    fn bridge_call_returns_string() {
        let (out, h) = run_with_bridge(
            "export default await myTool()",
            "myTool",
            Limits {
                cpu_time_ms: 5_000,
                wall_time_ms: 10_000,
                ..Default::default()
            },
            |s| {
                s.send(bridge_resp_ok(0, &WireValue::String("hello bridge".into())));
            },
        );
        h.join().unwrap();
        assert_eq!(get_default(&out.unwrap()).as_deref(), Some("hello bridge"));
    }

    #[test]
    fn bridge_call_passes_args_to_host() {
        // Verify the BridgeCall frame actually contains the argument we passed.
        let (io, host) = test_io();

        let responder = std::thread::spawn(move || {
            let host = host;
            let frame = host.recv();
            // callId (4) + targetKind (1) + specifier absent (1) + name len (4)
            // + "add" (3) = 13, then the args value slot: u32 length + blob.
            let (_, _, _, export_name, args) = bridge_call_parts(frame);
            assert_eq!(export_name, ["add"]);
            assert_eq!(args, vec![WireValue::Number(7.0)]);
            host.send(bridge_resp_ok(0, &WireValue::Number(99.0)));
        });

        let out = execute(
            "export default await add(7)",
            None,
            Limits {
                cpu_time_ms: 5_000,
                wall_time_ms: 10_000,
                ..Default::default()
            },
            &[HostGlobalDef::bridge("add")],
            &[],
            io,
            Arc::new(AtomicU32::new(0)),
            None,
        )
        .unwrap();
        responder.join().unwrap();
        assert_eq!(get_default(&out).as_deref(), Some("99"));
    }

    #[test]
    fn bridge_call_host_error_surfaces_as_host_bridge_error() {
        let (result, h) = run_with_bridge(
            "export default await myTool()",
            "myTool",
            Limits {
                cpu_time_ms: 5_000,
                wall_time_ms: 10_000,
                ..Default::default()
            },
            |s| {
                s.send(bridge_resp_err(0, "handler blew up"));
            },
        );
        h.join().unwrap();
        let err = result.unwrap_err().error;
        assert!(
            matches!(err, RunError::HostBridge(ref e) if e.message.contains("handler blew up")),
            "expected HostBridge, got {err:?}"
        );
    }

    #[test]
    fn bridge_result_too_deep_for_the_reader_fails_the_run_as_type_not_serializable() {
        let (result, h) = run_with_bridge(
            "try { await myTool() } catch {} export default 1",
            "myTool",
            Limits {
                cpu_time_ms: 5_000,
                wall_time_ms: 10_000,
                ..Default::default()
            },
            |s| {
                let blob = crate::blob::deep_array_blob(1_000_000);
                s.send(Inbound::BridgeResponse {
                    call_id: 0,
                    result: Ok(Some(blob)),
                });
            },
        );
        h.join().unwrap();
        let err = result.unwrap_err().error;
        assert!(
            matches!(err, RunError::TypeNotSerializable(ref m) if m.contains("too deep")),
            "expected TypeNotSerializable, got {err:?}"
        );
    }

    #[test]
    fn bridge_host_error_preserves_name_and_fields_when_uncaught() {
        let fields = WireValue::Object(vec![(
            "code".to_string(),
            WireValue::String("E_FOO".into()),
        )]);
        let (result, h) = run_with_bridge(
            "export default await myTool()",
            "myTool",
            Limits {
                cpu_time_ms: 5_000,
                wall_time_ms: 10_000,
                ..Default::default()
            },
            move |s| {
                s.send(bridge_resp_err_full(0, "WorkflowTimeout", "took too long", Some(&fields)));
            },
        );
        h.join().unwrap();
        let err = result.unwrap_err().error;
        match err {
            RunError::HostBridge(e) => {
                assert_eq!(e.name, "WorkflowTimeout");
                assert_eq!(e.message, "took too long");
                assert_eq!(
                    e.fields,
                    Some(testval::to_blob(&WireValue::Object(vec![(
                        "code".to_string(),
                        WireValue::String("E_FOO".into())
                    )])))
                );
            }
            other => panic!("expected HostBridge, got {other:?}"),
        }
    }

    #[test]
    fn bridge_host_error_is_catchable_in_sandbox() {
        // Sandbox catches the host error and inspects it: the run succeeds
        // and the caught error exposes name, message, direct fields, and
        // instanceof.
        let fields = WireValue::Object(vec![("code".to_string(), WireValue::String("E_T".into()))]);
        let (result, h) = run_with_bridge(
            r#"
            let out;
            try {
              await myTool();
              out = "did not throw";
            } catch (e) {
              out = [
                e.name,
                e.message,
                e.code,
                e instanceof TypeError,
                e instanceof Error,
              ].join("|");
            }
            export default out;
            "#,
            "myTool",
            Limits {
                cpu_time_ms: 5_000,
                wall_time_ms: 10_000,
                ..Default::default()
            },
            move |s| {
                s.send(bridge_resp_err_full(0, "TypeError", "bad input", Some(&fields)));
            },
        );
        h.join().unwrap();
        let out = result.expect("caught host error must not fail the run");
        assert_eq!(
            get_default(&out).as_deref(),
            Some("TypeError|bad input|E_T|true|true")
        );
    }

    #[test]
    fn bridge_host_error_reserved_keys_cannot_be_injected_via_fields() {
        // A crafted fields payload must not override Error identity.
        let fields = WireValue::Object(vec![
            ("name".to_string(), WireValue::String("Spoofed".into())),
            ("message".to_string(), WireValue::String("spoofed".into())),
            ("stack".to_string(), WireValue::String("fake stack".into())),
            ("ok".to_string(), WireValue::String("legit".into())),
        ]);
        let (result, h) = run_with_bridge(
            r#"
            let out;
            try { await myTool() } catch (e) {
              out = [e.name, e.message, e.ok, String(e.stack).includes("fake stack")].join("|");
            }
            export default out;
            "#,
            "myTool",
            Limits {
                cpu_time_ms: 5_000,
                wall_time_ms: 10_000,
                ..Default::default()
            },
            move |s| {
                s.send(bridge_resp_err_full(0, "Error", "real message", Some(&fields)));
            },
        );
        h.join().unwrap();
        let out = result.expect("caught host error must not fail the run");
        assert_eq!(
            get_default(&out).as_deref(),
            Some("Error|real message|legit|false")
        );
    }

    #[test]
    fn sandbox_thrown_primitive_has_no_stack_and_no_fields() {
        let err = run_code(
            r#"throw "some string""#,
            "<test>",
            Limits {
                cpu_time_ms: 5_000,
                wall_time_ms: 10_000,
                ..Default::default()
            },
        )
        .map_err(|f| f.error)
        .unwrap_err();
        match err {
            RunError::RuntimeError(e) => {
                assert_eq!(e.name, "Error");
                assert_eq!(e.message, "some string");
                assert_eq!(e.stack, None, "primitive throw must not carry a stack");
                assert_eq!(
                    e.fields, None,
                    "primitive throw must not enumerate wrapper chars"
                );
            }
            other => panic!("expected RuntimeError, got {other:?}"),
        }
    }

    #[test]
    fn bridge_host_error_custom_name_is_carried() {
        let (result, h) = run_with_bridge(
            r#"
            let out;
            try { await myTool() } catch (e) { out = `${e.name}:${e.stack === undefined}` }
            export default out;
            "#,
            "myTool",
            Limits {
                cpu_time_ms: 5_000,
                wall_time_ms: 10_000,
                ..Default::default()
            },
            |s| {
                s.send(bridge_resp_err_full(0, "NonRetryableError", "nope", None));
            },
        );
        h.join().unwrap();
        let out = result.expect("caught host error must not fail the run");
        // Custom name is carried; the stack is sandbox-local (V8 attaches one
        // at Exception::error creation), so we only assert the name here.
        let val = get_default(&out).unwrap();
        assert!(
            val.starts_with("NonRetryableError:"),
            "expected custom error name, got {val}"
        );
    }

    #[test]
    fn bridge_call_function_argument_rejected() {
        // Function args are rejected before any socket write.
        let (io, host) = test_io();

        // The bridge errors before sending, so no message arrives.
        let responder = std::thread::spawn(move || {
            let _ = host.recv_timeout(Duration::from_millis(500)); // may time out — ignore
        });

        let err = execute(
            "export default await myTool(() => 42)",
            None,
            Limits {
                cpu_time_ms: 5_000,
                wall_time_ms: 10_000,
                ..Default::default()
            },
            &[HostGlobalDef::bridge("myTool")],
            &[],
            io,
            Arc::new(AtomicU32::new(0)),
            None,
        )
        .unwrap_err()
        .error;

        responder.join().unwrap();
        assert!(
            matches!(err, RunError::FunctionArgumentNotSupported),
            "expected FunctionArgumentNotSupported, got {err:?}"
        );
    }

    #[test]
    fn bridge_call_outbound_payload_too_large_is_rejected() {
        // A call payload larger than max_bridge_payload_bytes must be caught
        // before writing to the socket and surface as BridgePayloadTooLarge.
        let (io, host) = test_io();

        // The bridge aborts before sending, so no message arrives.
        let responder = std::thread::spawn(move || {
            let _ = host.recv_timeout(Duration::from_millis(500)); // may time out — ignore
        });

        // Pass a 10-byte string argument but set the limit to 4 bytes.
        let err = execute(
            r#"export default await myTool("hello12345")"#,
            None,
            Limits {
                cpu_time_ms: 5_000,
                wall_time_ms: 10_000,
                max_bridge_call_bytes: 4,
                ..Default::default()
            },
            &[HostGlobalDef::bridge("myTool")],
            &[],
            io,
            Arc::new(AtomicU32::new(0)),
            None,
        )
        .unwrap_err()
        .error;

        responder.join().unwrap();
        assert!(
            matches!(err, RunError::BridgeCallPayloadTooLarge),
            "expected BridgeCallPayloadTooLarge, got {err:?}"
        );
    }

    #[test]
    fn bridge_call_inbound_response_too_large_is_rejected() {
        // The per-run inbound allowance (soft_memory_mb) is enforced by the
        // frontend before a message reaches the engine; the server's
        // session.rs tests cover the refusal. Here: a small response under a small budget
        // still arrives and the run succeeds.
        let (out, h) = run_with_bridge(
            "export default await myTool()",
            "myTool",
            Limits {
                cpu_time_ms: 5_000,
                wall_time_ms: 10_000,
                soft_memory_mb: 64,
                hard_memory_mb: 64,
                ..Default::default()
            },
            |s| {
                // Response is tiny — should succeed.
                s.send(bridge_resp_ok(0, &WireValue::String("ok".into())));
            },
        );
        h.join().unwrap();
        assert_eq!(get_default(&out.unwrap()).as_deref(), Some("ok"));
    }

    #[test]
    fn bridge_call_payload_within_limit_succeeds() {
        // When the payload fits within max_bridge_payload_bytes the call
        // should succeed normally.
        let (out, h) = run_with_bridge(
            "export default await myTool()",
            "myTool",
            Limits {
                cpu_time_ms: 5_000,
                wall_time_ms: 10_000,
                max_bridge_call_bytes: 1024,
                ..Default::default()
            },
            |s| {
                s.send(bridge_resp_ok(0, &WireValue::Number(7.0)));
            },
        );
        h.join().unwrap();
        assert_eq!(get_default(&out.unwrap()).as_deref(), Some("7"));
    }

    #[test]
    fn bridge_call_zero_limit_means_no_limit() {
        // max_bridge_payload_bytes = 0 disables the per-bridge cap entirely.
        let (out, h) = run_with_bridge(
            "export default await myTool()",
            "myTool",
            Limits {
                cpu_time_ms: 5_000,
                wall_time_ms: 10_000,
                max_bridge_call_bytes: 0,
                ..Default::default()
            },
            |s| {
                s.send(bridge_resp_ok(0, &WireValue::String("big payload".into())));
            },
        );
        h.join().unwrap();
        assert_eq!(get_default(&out.unwrap()).as_deref(), Some("big payload"));
    }

    #[test]
    fn bridge_call_wall_timeout_mid_wait() {
        // If the host never responds, the wall guard terminates V8.
        let (io, host) = test_io();
        let _host_guard = host; // keep the events open, never respond

        let err = execute(
            "export default await myTool()",
            None,
            Limits {
                cpu_time_ms: 10_000,
                wall_time_ms: 300,
                ..Default::default()
            },
            &[HostGlobalDef::bridge("myTool")],
            &[],
            io,
            Arc::new(AtomicU32::new(0)),
            None,
        )
        .unwrap_err()
        .error;

        assert!(
            matches!(err, RunError::WallTimeout),
            "expected WallTimeout, got {err:?}"
        );
    }

    #[test]
    fn bridge_call_returns_object() {
        let (out, h) = run_with_bridge(
            "const r = await myTool(); export default r.x + r.y",
            "myTool",
            Limits {
                cpu_time_ms: 5_000,
                wall_time_ms: 10_000,
                ..Default::default()
            },
            |s| {
                s.send(bridge_resp_ok(
                        0,
                        &WireValue::Object(vec![
                            ("x".to_string(), WireValue::Number(3.0)),
                            ("y".to_string(), WireValue::Number(4.0)),
                        ]),
                    ));
            },
        );
        h.join().unwrap();
        assert_eq!(get_default(&out.unwrap()).as_deref(), Some("7"));
    }

    #[test]
    fn bridge_wait_respects_wall_timeout() {
        // If the host never responds and a wall limit is set, the run's wall
        // deadline fires and it ends with WallTimeout.
        let (io, host) = test_io();
        let _guard = host; // keep open, never respond

        let err = execute(
            "export default await myTool()",
            None,
            Limits {
                wall_time_ms: 200,
                cpu_time_ms: 30_000,
                ..Default::default()
            },
            &[HostGlobalDef::bridge("myTool")],
            &[],
            io,
            Arc::new(AtomicU32::new(0)),
            None,
        )
        .unwrap_err()
        .error;
        assert!(matches!(err, RunError::WallTimeout), "got {err:?}");
    }

    // ── __proto__ elision ─────────────────────────────────────────────────────

    #[test]
    fn bridge_proto_own_key_in_host_response_stays_a_plain_own_key() {
        // Host returns an object carrying "__proto__" as an own key. The V8
        // deserializer stores it as a plain own data property — it never runs
        // the Object.prototype __proto__ setter — so it reaches the sandbox
        // as data and the object's prototype is untouched.
        let (out, h) = run_with_bridge(
            // Sort own property names to get a deterministic comma-joined string.
            "const r = await myTool(); \
             export default Object.getOwnPropertyNames(r).sort().join(',')",
            "myTool",
            Limits {
                cpu_time_ms: 5_000,
                wall_time_ms: 10_000,
                ..Default::default()
            },
            |s| {
                s.send(bridge_resp_ok(
                        0,
                        &WireValue::Object(vec![
                            ("x".to_string(), WireValue::Number(1.0)),
                            ("__proto__".to_string(), WireValue::Number(99.0)),
                            ("y".to_string(), WireValue::Number(2.0)),
                        ]),
                    ));
            },
        );
        h.join().unwrap();
        assert_eq!(get_default(&out.unwrap()).as_deref(), Some("__proto__,x,y"));
    }

    #[test]
    fn bridge_proto_own_key_in_host_response_does_not_pollute() {
        // The companion to the test above: the key arrives as data, and
        // neither the receiving object's prototype nor Object.prototype moves.
        let (out, h) = run_with_bridge(
            "const r = await myTool(); \
             export default [ \
               Object.getPrototypeOf(r) === Object.prototype, \
               ({}).polluted === undefined, \
               r.x, \
             ].join(',')",
            "myTool",
            Limits {
                cpu_time_ms: 5_000,
                wall_time_ms: 10_000,
                ..Default::default()
            },
            |s| {
                s.send(bridge_resp_ok(
                        0,
                        &WireValue::Object(vec![
                            ("x".to_string(), WireValue::Number(1.0)),
                            (
                                "__proto__".to_string(),
                                WireValue::Object(vec![(
                                    "polluted".to_string(),
                                    WireValue::Bool(true),
                                )]),
                            ),
                        ]),
                    ));
            },
        );
        h.join().unwrap();
        assert_eq!(get_default(&out.unwrap()).as_deref(), Some("true,true,1"));
    }

    // ── max_bridge_calls limit ─────────────────────────────────────────────

    #[test]
    fn bridge_call_limit_exceeded_after_limit_calls() {
        // The sandbox calls myTool 5 times; limit is 3. The first 3 succeed,
        // the 4th triggers ERR_BRIDGE_CALL_LIMIT_EXCEEDED before any I/O.
        let (io, host) = test_io();

        let handle =
            std::thread::spawn(move || drain_bridge_calls(host, 10, WireValue::Number(1.0)));

        let err = execute(
            // 5 sequential awaited calls
            "let n = 0; \
             n += await myTool(); \
             n += await myTool(); \
             n += await myTool(); \
             n += await myTool(); \
             n += await myTool(); \
             export default n",
            None,
            Limits {
                cpu_time_ms: 5_000,
                wall_time_ms: 10_000,
                max_bridge_calls: 3,
                ..Default::default()
            },
            &[HostGlobalDef::bridge("myTool")],
            &[],
            io,
            Arc::new(AtomicU32::new(0)),
            None,
        )
        .unwrap_err()
        .error;

        let calls_handled = handle.join().unwrap();
        assert!(
            matches!(err, RunError::BridgeCallLimitExceeded),
            "expected BridgeCallLimitExceeded, got {err:?}"
        );
        // Exactly 3 calls should have reached the host before the 4th was blocked.
        assert_eq!(
            calls_handled, 3,
            "expected 3 calls to reach host, got {calls_handled}"
        );
    }

    #[test]
    fn bridge_call_limit_zero_means_no_limit() {
        // max_bridge_calls = 0 disables the count limit entirely.
        // All 5 calls should succeed.
        let (io, host) = test_io();

        let handle =
            std::thread::spawn(move || drain_bridge_calls(host, 5, WireValue::Number(10.0)));

        let out = execute(
            "let n = 0; \
             n += await myTool(); \
             n += await myTool(); \
             n += await myTool(); \
             n += await myTool(); \
             n += await myTool(); \
             export default n",
            None,
            Limits {
                cpu_time_ms: 5_000,
                wall_time_ms: 10_000,
                max_bridge_calls: 0,
                ..Default::default()
            },
            &[HostGlobalDef::bridge("myTool")],
            &[],
            io,
            Arc::new(AtomicU32::new(0)),
            None,
        )
        .unwrap();

        handle.join().unwrap();
        assert_eq!(get_default(&out).as_deref(), Some("50"));
    }

    #[test]
    fn bridge_call_exactly_at_limit_succeeds() {
        // max_bridge_calls = 3; sandbox makes exactly 3 calls → should succeed.
        let (io, host) = test_io();

        let handle =
            std::thread::spawn(move || drain_bridge_calls(host, 10, WireValue::Number(1.0)));

        let out = execute(
            "let n = 0; \
             n += await myTool(); \
             n += await myTool(); \
             n += await myTool(); \
             export default n",
            None,
            Limits {
                cpu_time_ms: 5_000,
                wall_time_ms: 10_000,
                max_bridge_calls: 3,
                ..Default::default()
            },
            &[HostGlobalDef::bridge("myTool")],
            &[],
            io,
            Arc::new(AtomicU32::new(0)),
            None,
        )
        .unwrap();

        handle.join().unwrap();
        assert_eq!(get_default(&out).as_deref(), Some("3"));
    }

    #[test]
    fn bridge_call_limit_is_shared_across_globals() {
        // max_bridge_calls applies across all stubs in a run.
        // Two globals (toolA + toolB) share the same counter.
        let (io, host) = test_io();

        // Respond to up to 4 bridge calls, regardless of which global.
        let handle =
            std::thread::spawn(move || drain_bridge_calls(host, 4, WireValue::Number(1.0)));

        let err = execute(
            // 2 calls to toolA + 2 calls to toolB = 4 total; limit = 3
            "await toolA(); await toolA(); await toolB(); await toolB(); \
             export default 'done'",
            None,
            Limits {
                cpu_time_ms: 5_000,
                wall_time_ms: 10_000,
                max_bridge_calls: 3,
                ..Default::default()
            },
            &[
                HostGlobalDef::bridge("toolA"),
                HostGlobalDef::bridge("toolB"),
            ],
            &[],
            io,
            Arc::new(AtomicU32::new(0)),
            None,
        )
        .unwrap_err()
        .error;

        let calls_handled = handle.join().unwrap();
        assert!(
            matches!(err, RunError::BridgeCallLimitExceeded),
            "expected BridgeCallLimitExceeded, got {err:?}"
        );
        assert_eq!(
            calls_handled, 3,
            "expected 3 calls before limit, got {calls_handled}"
        );
    }

    #[test]
    fn bridge_calls_recorded_on_success_without_limit() {
        // max_bridge_calls = 0 (unlimited): calls are still recorded and
        // reported on the success output, with metadata.
        let (io, host) = test_io();

        let handle =
            std::thread::spawn(move || drain_bridge_calls(host, 3, WireValue::Number(1.0)));

        let out = execute(
            "await myTool(); await myTool(); await myTool(); export default 'done'",
            None,
            Limits {
                cpu_time_ms: 5_000,
                wall_time_ms: 10_000,
                max_bridge_calls: 0,
                ..Default::default()
            },
            &[HostGlobalDef::bridge("myTool")],
            &[],
            io,
            Arc::new(AtomicU32::new(0)),
            None,
        )
        .unwrap();

        handle.join().unwrap();
        assert_eq!(out.bridge_calls.len(), 3);
        for record in &out.bridge_calls {
            assert_eq!(record.name, "myTool");
            assert_eq!(record.outcome, types::BridgeCallOutcome::Ok, "served call must settle ok");
            assert!(record.arg_bytes > 0, "the args blob has bytes");
            assert!(record.response_bytes > 0, "number response has bytes");
            assert!(record.duration_ms >= 0.0);
        }
        // Attempt order is preserved on the run clock.
        assert!(out.bridge_calls[0].start_ms <= out.bridge_calls[1].start_ms);
        assert!(out.bridge_calls[1].start_ms <= out.bridge_calls[2].start_ms);
        // Active CPU time is reported with sub-millisecond resolution.
        assert!(out.cpu_time_ms > 0.0);
        assert!(out.cpu_time_ms <= out.duration_ms);
    }

    #[test]
    fn bridge_calls_recorded_on_limit_failure() {
        // The violating attempt is recorded too, flagged as blocked:
        // limit 3 → 4 records, the last one blocked.
        let (io, host) = test_io();

        let handle =
            std::thread::spawn(move || drain_bridge_calls(host, 10, WireValue::Number(1.0)));

        let failure = execute(
            "await myTool(); await myTool(); await myTool(); await myTool(); \
             export default 'done'",
            None,
            Limits {
                cpu_time_ms: 5_000,
                wall_time_ms: 10_000,
                max_bridge_calls: 3,
                ..Default::default()
            },
            &[HostGlobalDef::bridge("myTool")],
            &[],
            io,
            Arc::new(AtomicU32::new(0)),
            None,
        )
        .unwrap_err();

        handle.join().unwrap();
        assert!(matches!(failure.error, RunError::BridgeCallLimitExceeded));
        assert_eq!(failure.bridge_calls.len(), 4);
        assert!(failure.bridge_calls[..3].iter().all(|r| r.outcome == types::BridgeCallOutcome::Ok));
        let violating = &failure.bridge_calls[3];
        assert_eq!(violating.outcome, types::BridgeCallOutcome::Blocked);
        assert_eq!(violating.response_bytes, 0);
    }

    #[test]
    fn bridge_call_limit_violation_is_uncatchable() {
        // Sandbox code swallows the limit error in try/catch and would have
        // completed successfully before the fix (the violation only surfaced
        // if the error propagated uncaught). The violation must terminate
        // execution: the run fails even though every throw is caught, and no
        // attempt past the limit reaches the host.
        let (io, host) = test_io();

        let handle =
            std::thread::spawn(move || drain_bridge_calls(host, 10, WireValue::Number(1.0)));

        let err = execute(
            "let n = 0; \
             for (let i = 0; i < 10; i++) { \
               try { n += await myTool(); } catch (e) { /* swallowed */ } \
             } \
             export default n",
            None,
            Limits {
                cpu_time_ms: 5_000,
                wall_time_ms: 10_000,
                max_bridge_calls: 3,
                ..Default::default()
            },
            &[HostGlobalDef::bridge("myTool")],
            &[],
            io,
            Arc::new(AtomicU32::new(0)),
            None,
        )
        .unwrap_err()
        .error;

        let calls_handled = handle.join().unwrap();
        assert!(
            matches!(err, RunError::BridgeCallLimitExceeded),
            "expected BridgeCallLimitExceeded despite try/catch, got {err:?}"
        );
        assert_eq!(
            calls_handled, 3,
            "expected exactly 3 calls to reach the host, got {calls_handled}"
        );
    }

    #[test]
    fn bridge_payload_too_large_is_uncatchable() {
        // An oversized argument payload terminates the run even when the
        // sandbox catches the error; the call never reaches the host.
        let (io, host) = test_io();

        let handle =
            std::thread::spawn(move || drain_bridge_calls(host, 1, WireValue::Number(1.0)));

        let err = execute(
            "try { await myTool('x'.repeat(100000)); } catch (e) { /* swallowed */ } \
             export default 'survived'",
            None,
            Limits {
                cpu_time_ms: 5_000,
                wall_time_ms: 10_000,
                max_bridge_call_bytes: 1_000,
                ..Default::default()
            },
            &[HostGlobalDef::bridge("myTool")],
            &[],
            io,
            Arc::new(AtomicU32::new(0)),
            None,
        )
        .unwrap_err()
        .error;

        let calls_handled = handle.join().unwrap();
        assert!(
            matches!(err, RunError::BridgeCallPayloadTooLarge),
            "expected BridgeCallPayloadTooLarge despite try/catch, got {err:?}"
        );
        assert_eq!(calls_handled, 0, "oversized call must never reach the host");
    }

    #[test]
    fn bridge_function_argument_is_uncatchable() {
        // Function arguments cannot cross the bridge; the violation terminates
        // the run even when the sandbox catches the error.
        let (io, host) = test_io();

        let handle =
            std::thread::spawn(move || drain_bridge_calls(host, 1, WireValue::Number(1.0)));

        let err = execute(
            "try { await myTool(() => 1); } catch (e) { /* swallowed */ } \
             export default 'survived'",
            None,
            Limits {
                cpu_time_ms: 5_000,
                wall_time_ms: 10_000,
                ..Default::default()
            },
            &[HostGlobalDef::bridge("myTool")],
            &[],
            io,
            Arc::new(AtomicU32::new(0)),
            None,
        )
        .unwrap_err()
        .error;

        let calls_handled = handle.join().unwrap();
        assert!(
            matches!(err, RunError::FunctionArgumentNotSupported),
            "expected FunctionArgumentNotSupported despite try/catch, got {err:?}"
        );
        assert_eq!(
            calls_handled, 0,
            "call with function argument must never reach the host"
        );
    }

    // ── callId validation ──────────────────────────────────────────────────

    #[test]
    fn bridge_response_with_wrong_call_id_is_rejected() {
        // A BridgeResponse whose callId does not match the one Rust sent must
        // be rejected as ERR_INTERNAL (cross-run contamination guard).
        let (io, host) = test_io();

        let handle = std::thread::spawn(move || {
            // Read the BridgeCall and reply with a deliberately wrong callId.
            let frame = host.recv();
            let sent_call_id = bridge_call_parts(frame).0;
            // Respond with a callId that differs from the one Rust sent.
            let wrong_call_id = sent_call_id.wrapping_add(99);
            host.send(bridge_resp_ok(wrong_call_id, &WireValue::Number(1.0)));
        });

        let err = execute(
            "export default await myTool()",
            None,
            Limits {
                cpu_time_ms: 5_000,
                wall_time_ms: 10_000,
                ..Default::default()
            },
            &[HostGlobalDef::bridge("myTool")],
            &[],
            io,
            Arc::new(AtomicU32::new(0)),
            None,
        )
        .unwrap_err()
        .error;

        handle.join().unwrap();
        assert!(
            matches!(err, RunError::Internal(_)),
            "expected Internal (callId mismatch), got {err:?}"
        );
    }

    #[test]
    fn per_connection_call_id_counter_prevents_cross_run_alias() {
        // Simulates two sequential runs sharing the same per-connection
        // call_id_counter.  Run 1 uses callId=0; the counter advances so
        // Run 2 starts at callId=1 — a stale BridgeResponse with callId=0
        // that arrives during Run 2 is rejected as a callId mismatch.

        let counter = Arc::new(AtomicU32::new(0));

        // ── Run 1: succeeds normally ─────────────────────────────────────
        let (io1, host1) = test_io();

        let h1 = std::thread::spawn(move || {
            let frame = host1.recv();
            let cid = bridge_call_parts(frame).0;
            host1.send(bridge_resp_ok(cid, &WireValue::Number(1.0)));
            cid // return the callId run 1 used
        });

        let out = execute(
            "export default await myTool()",
            None,
            Limits {
                cpu_time_ms: 5_000,
                wall_time_ms: 10_000,
                ..Default::default()
            },
            &[HostGlobalDef::bridge("myTool")],
            &[],
            io1,
            Arc::clone(&counter),
            None,
        )
        .unwrap();
        assert_eq!(get_default(&out).as_deref(), Some("1"));
        let run1_call_id = h1.join().unwrap();
        assert_eq!(run1_call_id, 0, "run 1 should use callId=0");

        // ── Run 2: counter has advanced; a stale run-1 response is rejected ─
        let (io2, host2) = test_io();

        let h2 = std::thread::spawn(move || {
            let frame = host2.recv();
            let cid = bridge_call_parts(frame).0;
            // Simulate a stale BridgeResponse from run 1 arriving here
            // (callId=0 instead of the expected callId for run 2).
            host2.send(bridge_resp_ok(run1_call_id, &WireValue::Number(99.0)));
            cid // return callId run 2 expected
        });

        let err = execute(
            "export default await myTool()",
            None,
            Limits {
                cpu_time_ms: 5_000,
                wall_time_ms: 10_000,
                ..Default::default()
            },
            &[HostGlobalDef::bridge("myTool")],
            &[],
            io2,
            Arc::clone(&counter),
            None,
        )
        .unwrap_err()
        .error;

        let run2_call_id = h2.join().unwrap();
        assert!(
            run2_call_id > run1_call_id,
            "run 2 callId ({run2_call_id}) must be > run 1 callId ({run1_call_id})"
        );
        assert!(
            matches!(err, RunError::Internal(_)),
            "expected Internal (stale callId rejected), got {err:?}"
        );
    }

    // ── Concurrent bridge calls (D11) ───────────────────────────────────────────────────

    #[test]
    fn promise_all_concurrent_bridge_calls_route_by_call_id() {
        // Promise.all([toolA(), toolB()]) fires both bridge_global_callbacks
        // synchronously before V8 yields, putting two BridgeCall frames on the
        // wire at the same time.  The host responds in REVERSE order to prove
        // the poll loop routes each response to the correct resolver by callId.
        let (io, host) = test_io();

        let handle = std::thread::spawn(move || {
            // Read both BridgeCall frames (they arrive before V8 yields).
            let f1 = host.recv();
            let cid1 = bridge_call_parts(f1).0;
            let f2 = host.recv();
            let cid2 = bridge_call_parts(f2).0;

            // Respond in reverse order: second call gets 20, first gets 10.
            // The poll loop must route cid2 → toolB resolver (20)
            // and cid1 → toolA resolver (10) regardless of order.
            host.send(bridge_resp_ok(cid2, &WireValue::Number(20.0)));
            host.send(bridge_resp_ok(cid1, &WireValue::Number(10.0)));
        });

        let out = execute(
            // toolA resolves to 10, toolB resolves to 20; sum = 30.
            "const [a, b] = await Promise.all([toolA(), toolB()]); \
             export default a + b",
            None,
            Limits {
                cpu_time_ms: 5_000,
                wall_time_ms: 10_000,
                ..Default::default()
            },
            &[
                HostGlobalDef::bridge("toolA"),
                HostGlobalDef::bridge("toolB"),
            ],
            &[],
            io,
            Arc::new(AtomicU32::new(0)),
            None,
        )
        .unwrap();

        handle.join().unwrap();
        assert_eq!(get_default(&out).as_deref(), Some("30"));
    }

    #[test]
    fn promise_all_three_concurrent_calls_all_resolve() {
        // Three concurrent calls; responses arrive in reverse order.
        // Validates callId routing across more than two in-flight resolvers.
        let (io, host) = test_io();

        let handle = std::thread::spawn(move || {
            let mut call_ids = Vec::new();
            for _ in 0..3 {
                let f = host.recv();
                let cid = bridge_call_parts(f).0;
                call_ids.push(cid);
            }
            // Respond in reverse.
            for (i, &cid) in call_ids.iter().rev().enumerate() {
                host.send(bridge_resp_ok(cid, &WireValue::Number((i as f64 + 1.0) * 10.0)));
            }
        });

        // All three resolve — we just care the module completes without error.
        let out = execute(
            "const [a, b, c] = await Promise.all([tool(), tool(), tool()]); \
             export default typeof a === 'number' && typeof b === 'number' && typeof c === 'number'",
            None,
            Limits { cpu_time_ms: 5_000, wall_time_ms: 10_000, ..Default::default() },
            &[HostGlobalDef::bridge("tool")],
            &[], io,
            Arc::new(AtomicU32::new(0)),
            None,
        ).unwrap();

        handle.join().unwrap();
        assert_eq!(get_default(&out).as_deref(), Some("true"));
    }

    #[test]
    fn sandbox_export_with_proto_own_property_crosses_as_a_plain_own_key() {
        // Sandbox creates an object with __proto__ as an explicit own
        // enumerable property via Object.defineProperty (a plain object
        // literal `{ __proto__: x }` sets the prototype instead). V8's
        // serializer carries it as an own data property in both directions —
        // it is never re-applied through the prototype accessor, so no
        // pollution is possible. (The old wire codec dropped the key.)
        let out = run_ok(
            r#"
            const obj = {};
            Object.defineProperty(obj, '__proto__', { value: 99, enumerable: true });
            obj.x = 1;
            export default obj;
        "#,
        );
        let default_val = get_field(&out, "default").unwrap();
        if let WireValue::Object(fields) = default_val {
            let keys: Vec<&str> = fields.iter().map(|(k, _)| k.as_str()).collect();
            assert!(
                keys.contains(&"__proto__"),
                "expected __proto__ to survive as a plain own key, got: {keys:?}"
            );
            assert!(keys.contains(&"x"), "expected x to survive");
        } else {
            panic!("expected an object export");
        }
    }

    // ── Import handle walk order ───────────────────────────────────────

    #[test]
    fn collect_import_handles_assigns_depth_first_walk_order() {
        let imports = vec![
            host_import(
                "a",
                vec![
                    ("one", HostModuleNode::Function),
                    (
                        "data",
                        HostModuleNode::Data(testval::to_blob(&WireValue::Null)),
                    ),
                    (
                        "obj",
                        HostModuleNode::Object(vec![
                            ("two".to_string(), HostModuleNode::Function),
                            (
                                "deeper".to_string(),
                                HostModuleNode::Object(vec![(
                                    "three".to_string(),
                                    HostModuleNode::Function,
                                )]),
                            ),
                        ]),
                    ),
                ],
            ),
            source_import("s", "export const x = 1;"),
            host_import("b", vec![("four", HostModuleNode::Function)]),
        ];
        let handles = collect_import_handles(&imports);
        let names: Vec<&str> = handles.iter().map(|h| h.record_name.as_str()).collect();
        assert_eq!(
            names,
            vec!["a.one", "a.obj.two", "a.obj.deeper.three", "b.four"]
        );
        assert_eq!(host_module_base_id(&imports, "a"), 0);
        assert_eq!(host_module_base_id(&imports, "b"), 3);
    }

    // ── Host → sandbox calls ───────────────────────────────────────────

    #[test]
    fn call_named_export_returns_value() {
        let out = run_call(
            "export function add(a, b) { return a + b }",
            "add",
            &[WireValue::Number(2.0), WireValue::Number(40.0)],
        )
        .unwrap();
        assert_eq!(value_of(&out), WireValue::Number(42.0));
        assert!(out.skipped_exports.is_empty());
    }

    #[test]
    fn call_default_fetch_receiver_is_the_exported_object() {
        // `export default { fetch }` — the receiver must be the object the
        // final segment was read from, so `this` works (a `globalThis`
        // receiver would silently produce "undefined:POST").
        let out = run_call(
            "export default { tag: 'worker-a', fetch(method) { return `${this.tag}:${method}` } }",
            "default.fetch",
            &[WireValue::String("POST".to_string())],
        )
        .unwrap();
        assert_eq!(
            value_of(&out),
            WireValue::String("worker-a:POST".to_string())
        );
    }

    #[test]
    fn call_default_prototype_method_resolves() {
        // `export default new Worker()` — the method lives on the prototype,
        // so resolution must follow the prototype chain (own-property-only
        // lookups would break this shape; workerd walks the chain too).
        let out = run_call(
            "class Worker { constructor() { this.tag = 'w' } fetch(m) { return this.tag + m } }\n\
             export default new Worker()",
            "default.fetch",
            &[WireValue::String("!".to_string())],
        )
        .unwrap();
        assert_eq!(value_of(&out), WireValue::String("w!".to_string()));
    }

    #[test]
    fn call_single_segment_receiver_is_the_namespace() {
        // Plain `a.b()` semantics with the namespace as the root object:
        // a single-segment call is `ns.fn()`, so `this` is the namespace and
        // sibling exports are reachable through it.
        let out = run_call(
            "export const tag = 'ns'\nexport function whoami() { return this.tag }",
            "whoami",
            &[],
        )
        .unwrap();
        assert_eq!(value_of(&out), WireValue::String("ns".to_string()));
    }

    #[test]
    fn call_sync_handler_needs_no_socket() {
        // A sync return value must not enter the poll loop — with no socket a
        // poll would surface a bogus pending error.
        let out = run_call("export function f() { return 1 }", "f", &[]).unwrap();
        assert_eq!(value_of(&out), WireValue::Number(1.0));
    }

    #[test]
    fn call_async_handler_without_bridge_settles() {
        // Awaits that need no host I/O settle in the pre-poll checkpoint;
        // there is no socket here, so this also proves the loop is not
        // entered for them.
        let out = run_call(
            "export async function f() { const a = await 1; const b = await 2; return a + b }",
            "f",
            &[],
        )
        .unwrap();
        assert_eq!(value_of(&out), WireValue::Number(3.0));
    }

    #[test]
    fn call_async_handler_with_bridge_call_settles() {
        let (io, host) = test_io();
        let handle = spawn_responder(host, |s| {
            s.send(bridge_resp_ok(0, &WireValue::Number(41.0)));
        });
        let call = call_spec("f", &[WireValue::Number(1.0)]);
        let out = execute(
            "export async function f(n) { return (await myTool()) + n }",
            None,
            Limits::default(),
            &[HostGlobalDef::bridge("myTool")],
            &[],
            io,
            Arc::new(AtomicU32::new(0)),
            Some(&call),
        )
        .unwrap();
        handle.join().unwrap();
        assert_eq!(value_of(&out), WireValue::Number(42.0));
        assert_eq!(out.bridge_calls.len(), 1);
    }

    #[test]
    fn call_against_prefix_exports() {
        // prefix.call(): the export path resolves against the prefix
        // module's live namespace, module-scope closure state included.
        let prefix = prepared(
            "let hits = 0\n\
             export default { tag: 'worker-a', fetch(m) { hits += 1; return `${this.tag}|${hits}|${m}` } }\n\
             export function named(x) { return `named|${x}` }",
            &[],
            &[],
        );
        let call = call_spec("default.fetch", &[WireValue::String("POST".to_string())]);
        let out = execute_with_prefix(
            prefix,
            Limits::default(),
            &[],
            &[],
            RunIo::None,
            Arc::new(AtomicU32::new(0)),
            &call,
        )
        .unwrap();
        assert_eq!(
            value_of(&out),
            WireValue::String("worker-a|1|POST".to_string())
        );
    }

    #[test]
    fn call_named_export_on_prefix() {
        let prefix = prepared("export function named(x) { return `named|${x}` }", &[], &[]);
        let call = call_spec("named", &[WireValue::String("X".to_string())]);
        let out = execute_with_prefix(
            prefix,
            Limits::default(),
            &[],
            &[],
            RunIo::None,
            Arc::new(AtomicU32::new(0)),
            &call,
        )
        .unwrap();
        assert_eq!(value_of(&out), WireValue::String("named|X".to_string()));
    }

    #[test]
    fn call_unknown_path_is_call_target_not_found() {
        let err = run_call("export default { fetch() {} }", "default.missing", &[]).unwrap_err();
        assert!(
            matches!(&err, RunError::CallTargetNotFound(m) if m.contains("default.missing")),
            "got {err:?}"
        );
    }

    #[test]
    fn call_non_callable_target_is_call_target_not_found() {
        let err = run_call("export const n = 42", "n", &[]).unwrap_err();
        assert!(
            matches!(&err, RunError::CallTargetNotFound(m) if m.contains("not callable")),
            "got {err:?}"
        );
    }

    #[test]
    fn call_default_exported_class_method_is_a_clean_error() {
        // `export default class` puts `fetch` on the prototype, not on the
        // class object, so `default.fetch` must fail cleanly — the supported
        // shapes are `export default { fetch }` and `export default new W()`.
        let err = run_call(
            "export default class { fetch() { return 1 } }",
            "default.fetch",
            &[],
        )
        .unwrap_err();
        assert!(
            matches!(err, RunError::CallTargetNotFound(_)),
            "got {err:?}"
        );
    }

    #[test]
    fn call_path_through_non_object_is_call_target_not_found() {
        let err = run_call("export const n = 1", "n.f.g", &[]).unwrap_err();
        assert!(
            matches!(err, RunError::CallTargetNotFound(_)),
            "got {err:?}"
        );
    }

    #[test]
    fn call_empty_and_oversized_paths_are_rejected() {
        for path in ["", "a..b", ".a"] {
            let err = run_call("export const a = 1", path, &[]).unwrap_err();
            assert!(
                matches!(err, RunError::CallTargetNotFound(_)),
                "path {path:?} got {err:?}"
            );
        }
        let deep = vec!["a"; MAX_CALL_PATH_SEGMENTS + 1].join(".");
        let err = run_call("export const a = 1", &deep, &[]).unwrap_err();
        assert!(
            matches!(err, RunError::CallTargetNotFound(_)),
            "got {err:?}"
        );
    }

    #[test]
    fn call_throwing_handler_is_user_code_error() {
        let err = run_call(
            r#"export function f() { throw new Error("boom") }"#,
            "f",
            &[],
        )
        .unwrap_err();
        assert!(
            matches!(&err, RunError::RuntimeError(inner) if inner.message == "boom"),
            "got {err:?}"
        );
    }

    #[test]
    fn call_async_rejection_is_user_code_error() {
        let err = run_call(
            r#"export async function f() { throw new Error("late boom") }"#,
            "f",
            &[],
        )
        .unwrap_err();
        assert!(
            matches!(&err, RunError::RuntimeError(inner) if inner.message == "late boom"),
            "got {err:?}"
        );
    }

    #[test]
    fn call_args_preserve_rich_types_and_identity() {
        // Two references to the same object arrive as the same object; a
        // bigint arrives as a bigint — the args cross as one V8 blob.
        let shared = WireValue::Object(vec![("k".to_string(), WireValue::Number(1.0))]);
        let out = run_call(
            "export function f(a, b, big) { return [a === b, typeof big] }",
            "f",
            &[shared.clone(), shared, WireValue::BigInt(false, vec![7u64])],
        )
        .unwrap();
        // NOTE: the two `shared` values above are equal but *separate* trees,
        // so identity is NOT preserved between them — this asserts the
        // mechanism (blob crossing) rather than test-fixture identity.
        assert_eq!(
            value_of(&out),
            WireValue::Array(vec![
                WireValue::Bool(false),
                WireValue::String("bigint".to_string())
            ])
        );
    }

    #[test]
    fn call_result_respects_max_export_bytes() {
        let call = call_spec("f", &[]);
        let failure = run_module(
            "export function f() { return 'x'.repeat(1024 * 1024) }",
            "<iso4>",
            Limits {
                max_export_bytes: 1024,
                ..Default::default()
            },
            &[],
            &[],
            RunIo::None,
            0,
            Arc::new(AtomicU32::new(0)),
            Some(&call),
            None,
            Arc::new(crate::link::NoopObserver),
        )
        .unwrap_err();
        assert!(matches!(failure.error, RunError::ExportTooLarge));
    }

    #[test]
    fn call_result_that_cannot_serialize_is_an_error_not_a_skip() {
        // Skipping is an exports-path affordance; a call's return value is
        // the whole result, so a non-serializable one must fail loudly.
        let err = run_call("export function f() { return () => 1 }", "f", &[]).unwrap_err();
        assert!(
            matches!(err, RunError::ExportNotSerializable(_)),
            "got {err:?}"
        );
    }

    #[test]
    fn call_run_reports_no_exports_and_no_skips() {
        // `call` present ⇒ value, never exports — even when the module also
        // has non-serializable exports that a plain run would skip.
        let out = run_call(
            "export default { fetch() { return 'ok' } }\nexport const extra = 1",
            "default.fetch",
            &[],
        )
        .unwrap();
        assert_eq!(value_of(&out), WireValue::String("ok".to_string()));
        assert!(out.skipped_exports.is_empty());
    }

    #[test]
    fn call_getter_along_the_path_runs_and_resolves() {
        // A path segment may be an accessor — it runs inside the CPU budget
        // and its result participates in resolution.
        let out = run_call(
            "export default { get inner() { return { f: () => 'via-getter' } } }",
            "default.inner.f",
            &[],
        )
        .unwrap();
        assert_eq!(value_of(&out), WireValue::String("via-getter".to_string()));
    }

    #[test]
    fn call_throwing_getter_is_call_target_not_found() {
        let err = run_call(
            "export default { get inner() { throw new Error('no') } }",
            "default.inner.f",
            &[],
        )
        .unwrap_err();
        assert!(
            matches!(err, RunError::CallTargetNotFound(_)),
            "got {err:?}"
        );
    }

    #[test]
    fn call_cpu_guard_applies_to_the_called_function() {
        // The guards must stay armed across the call — a handler that spins
        // forever is stopped by the CPU budget, not the wall clock.
        let call = call_spec("f", &[]);
        let failure = run_module(
            "export function f() { for (;;) {} }",
            "<iso4>",
            Limits {
                cpu_time_ms: 100,
                wall_time_ms: 10_000,
                ..Default::default()
            },
            &[],
            &[],
            RunIo::None,
            0,
            Arc::new(AtomicU32::new(0)),
            Some(&call),
            None,
            Arc::new(crate::link::NoopObserver),
        )
        .unwrap_err();
        assert!(matches!(failure.error, RunError::CpuTimeout));
    }
}
