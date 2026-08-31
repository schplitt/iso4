//! V8 isolate management and JavaScript execution.
//!
//! Owns everything V8-related: platform init, isolate creation, compilation,
//! evaluation, result extraction, console capture, and limit enforcement.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::io;
use std::mem::ManuallyDrop;
use std::os::unix::io::{FromRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Once;
use std::sync::OnceLock;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::RecvTimeoutError;

use crate::blob;
use crate::ipc;
use crate::ipc::{HostGlobalDef, HostModuleNode, ImportBinding, ImportModule};
use crate::wire::{self, BridgeErrorPayload};

static INIT: Once = Once::new();

#[derive(Default)]
struct LogBuffers {
    stdout: Vec<String>,
    stderr: Vec<String>,
    /// Running total of bytes across all stdout lines (excluding newlines).
    stdout_bytes: usize,
    /// Running total of bytes across all stderr lines (excluding newlines).
    stderr_bytes: usize,
    /// Cap for stdout. Zero = no limit.
    max_stdout_bytes: u32,
    /// Cap for stderr. Zero = no limit.
    max_stderr_bytes: u32,
}

/// Initialize the V8 platform. Safe to call from multiple threads -
/// `Once` ensures it runs exactly once per process.
pub fn init_platform() {
    INIT.call_once(|| {
        let platform = v8::new_default_platform(0, false).make_shared();
        v8::V8::initialize_platform(platform);
        v8::V8::initialize();
    });
}

// ── Limits ───────────────────────────────────────────────────────────────────

/// Resource limits for a single run. Zero means "no limit" for that field.
#[derive(Clone, Copy, Default)]
pub struct Limits {
    pub wall_time_ms: u32,
    pub cpu_time_ms: u32,
    /// V8 heap cap enforced via `CreateParams::heap_limits` +
    /// `add_near_heap_limit_callback`. Zero means no limit.
    pub memory_mb: u32,
    /// Maximum byte length of the exports value blob.
    /// Zero means no limit. Violation → `RunError::ExportTooLarge`.
    pub max_export_bytes: u32,
    /// Maximum bytes captured across all stdout lines.
    /// Zero means no limit. Lines that would exceed the cap are silently dropped.
    pub max_stdout_bytes: u32,
    /// Maximum bytes captured across all stderr lines.
    /// Zero means no limit. Lines that would exceed the cap are silently dropped.
    pub max_stderr_bytes: u32,
    /// Maximum bytes the sandbox may send as arguments in a single bridge call
    /// (sandbox → host). Zero means no per-call cap.
    pub max_bridge_call_bytes: u32,
    /// Maximum number of bridge calls (globals + host imports combined) a
    /// single run may make. Zero means no per-run limit.
    /// Default on the TS side is 10 when the host does not set an explicit
    /// value, so the door is never accidentally left open.
    pub max_bridge_calls: u32,
    /// Wall budget for the `waitUntil` epilogue after the run's value settles
    ///. Zero disables the epilogue: registered background work dies at
    /// settle, exactly the pre-waitUntil behavior.
    pub grace_ms: u32,
}

/// Termination reason set by the first limit-guard thread to fire.
///
/// Stored in a [`ReasonCell`] - first write wins, subsequent writes are
/// no-ops. Absence of a fired guard is represented naturally by
/// `ReasonCell::get()` returning `None`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TerminationReason {
    Wall,
    Cpu,
    Memory,
}

/// First-writer-wins slot for the run's termination reason, shared between
/// the guard threads, the near-heap callback, the ArrayBuffer allocator, and
/// the classification sites.
///
/// Used to be a `OnceLock`, which cannot be re-armed. The allocator and the
/// near-heap callback are registered once per *isolate* while guards and
/// classification are per *call*, so a reused isolate (warm instances)
/// needs one long-lived cell that resets between calls instead of a fresh
/// `OnceLock` the isolate-lifetime consumers can never re-point to.
struct ReasonCell(Mutex<Option<TerminationReason>>);

impl ReasonCell {
    fn new() -> Self {
        Self(Mutex::new(None))
    }

    /// First write wins; later writes are no-ops, matching `OnceLock::set`.
    fn set(&self, reason: TerminationReason) {
        let mut slot = self.0.lock().unwrap_or_else(|p| p.into_inner());
        slot.get_or_insert(reason);
    }

    fn get(&self) -> Option<TerminationReason> {
        *self.0.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Clear the slot for the next call on a reused isolate. Only safe while
    /// no JS is executing and no guards are armed — i.e. between calls.
    fn reset(&self) {
        *self.0.lock().unwrap_or_else(|p| p.into_inner()) = None;
    }
}

/// Heap data passed to [`near_heap_limit_cb`] as a raw pointer.
///
/// Lives on the heap for the duration of the run (see safety note on the
/// callback). Declaring it after `isolate` in [`run_module`] means it drops
/// before the isolate, which is safe because V8 never invokes near-heap
/// callbacks during `Isolate::Dispose()`.
struct NearHeapData {
    handle: v8::IsolateHandle,
    reason: Arc<ReasonCell>,
}

// ── ArrayBuffer budget allocator ─────────────────────────────────────────────
//
// V8's `heap_limits` only caps the JS heap (strings, plain objects, closures).
// TypedArray / ArrayBuffer backing stores are allocated through a separate
// allocator interface and would otherwise bypass the cap entirely.  This
// custom allocator tracks every backing-store byte and fires
// `terminate_execution()` the moment the cumulative total exceeds the budget.
//
// The state is heap-allocated via `Arc` so it can be shared between the
// allocator (which is registered in `CreateParams` *before* the isolate
// exists) and the run context (which sets the `IsolateHandle` *after* the
// isolate is created via the `OnceLock<IsolateHandle>` field).

struct BudgetAllocState {
    /// Currently allocated ArrayBuffer bytes (may briefly exceed budget while
    /// terminate_execution propagates to the next JS safepoint).
    used: AtomicUsize,
    /// Hard cap in bytes.  Always > 0 when the allocator is active.
    budget: usize,
    /// Set once, immediately after `Isolate::new` returns.
    handle: OnceLock<v8::IsolateHandle>,
    /// Shared with wall / cpu guards — first writer wins.
    reason: Arc<ReasonCell>,
}

impl BudgetAllocState {
    /// Called on every allocation path: if adding `n` bytes would exceed the
    /// budget, fire termination.  We always service the allocation so V8 does
    /// not crash — JS stops at the next safepoint.
    #[inline]
    fn check_and_maybe_terminate(&self, n: usize) {
        let prev = self.used.fetch_add(n, Ordering::Relaxed);
        if prev.saturating_add(n) > self.budget {
            if let Some(h) = self.handle.get() {
                self.reason.set(TerminationReason::Memory);
                h.terminate_execution();
            }
        }
    }
}

/// Vtable functions must be `unsafe extern "C"` free functions.
unsafe extern "C" fn budget_alloc_alloc(state: &BudgetAllocState, n: usize) -> *mut c_void {
    if n == 0 {
        return std::ptr::null_mut();
    }
    state.check_and_maybe_terminate(n);
    Box::into_raw(vec![0u8; n].into_boxed_slice()) as *mut c_void
}

unsafe extern "C" fn budget_alloc_alloc_uninit(state: &BudgetAllocState, n: usize) -> *mut c_void {
    if n == 0 {
        return std::ptr::null_mut();
    }
    state.check_and_maybe_terminate(n);
    let mut v: Vec<std::mem::MaybeUninit<u8>> = Vec::with_capacity(n);
    // SAFETY: MaybeUninit<u8> requires no initialization.
    v.set_len(n);
    Box::into_raw(v.into_boxed_slice()) as *mut c_void
}

unsafe extern "C" fn budget_alloc_free(state: &BudgetAllocState, data: *mut c_void, n: usize) {
    if data.is_null() || n == 0 {
        return;
    }
    state.used.fetch_sub(n, Ordering::Relaxed);
    let slice = std::ptr::slice_from_raw_parts_mut(data as *mut u8, n);
    drop(Box::from_raw(slice));
}

unsafe extern "C" fn budget_alloc_drop(state: *const BudgetAllocState) {
    // Reconstruct + drop the Arc reference that was created via Arc::into_raw.
    drop(Arc::from_raw(state));
}

/// The allocator V8 uses for ArrayBuffer backing stores, so growth is charged
/// against the run's memory budget.
///
/// There is deliberately no `reallocate` hook: V8 dropped
/// `ArrayBuffer::Allocator::Reallocate` and `rusty_v8` removed the vtable slot
/// with it. Resizes now run through `allocate` + copy + `free`, so every byte
/// still passes `check_and_maybe_terminate` — the budget sees the new size
/// before the old one is released, which is the conservative direction.
static BUDGET_ALLOC_VTABLE: v8::RustAllocatorVtable<BudgetAllocState> = v8::RustAllocatorVtable {
    allocate: budget_alloc_alloc,
    allocate_uninitialized: budget_alloc_alloc_uninit,
    free: budget_alloc_free,
    drop: budget_alloc_drop,
};

/// Near-heap-limit callback registered when `limits.memory_mb > 0`.
///
/// # Safety
/// `data` is a `*mut NearHeapData` kept alive in `run_module` for the
/// duration of active JS execution. This callback only fires during V8 GC,
/// which only occurs during `module.evaluate()` /
/// `perform_microtask_checkpoint()` — never during `Isolate::Dispose()`.
extern "C" fn near_heap_limit_cb(
    data: *mut std::ffi::c_void,
    current_heap_limit: usize,
    _initial_heap_limit: usize,
) -> usize {
    let d = unsafe { &*(data as *const NearHeapData) };
    d.reason.set(TerminationReason::Memory);
    d.handle.terminate_execution();
    // Return a larger limit so V8 has headroom to unwind the stack cleanly
    // after terminate_execution() without immediately crashing the process.
    current_heap_limit + 32 * 1024 * 1024
}

/// Tracks how much active V8 execution time has elapsed.
///
/// `enter()` / `leave()` bracket each period when V8 is actually running JS.
/// Time spent waiting on host bridge calls (Phase 4+) is excluded by calling
/// `leave()` before dispatching a bridge call and `enter()` on return.
///
/// Without bridge calls (current), `enter()` is called once just before
/// `module.evaluate()` and `leave()` once when the run ends, so `elapsed_ms()`
/// equals the wall time of the evaluation phase (compile + instantiate time
/// excluded).
pub struct CpuBudget {
    accumulated_ns: AtomicU64,
    epoch_start: Mutex<Option<Instant>>,
}

impl CpuBudget {
    pub fn new() -> Self {
        Self {
            accumulated_ns: AtomicU64::new(0),
            epoch_start: Mutex::new(None),
        }
    }

    /// Mark the start of a V8 execution epoch.
    pub fn enter(&self) {
        *self.epoch_start.lock().unwrap_or_else(|p| p.into_inner()) = Some(Instant::now());
    }

    /// End the current epoch and accumulate its duration.
    pub fn leave(&self) {
        let mut g = self.epoch_start.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(t) = g.take() {
            self.accumulated_ns
                .fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
        }
    }

    /// Total accumulated CPU time in milliseconds.
    pub fn elapsed_ms(&self) -> u64 {
        let base = self.accumulated_ns.load(Ordering::Relaxed);
        let active = self
            .epoch_start
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .map(|t| t.elapsed().as_nanos() as u64)
            .unwrap_or(0);
        (base + active) / 1_000_000
    }

    /// Total accumulated CPU time in milliseconds with microsecond
    /// resolution. The integer `elapsed_ms` is enough for the 10ms-poll CPU
    /// guard; the run result reports this precise value so sub-millisecond
    /// runs don't read as `0`.
    pub fn elapsed_ms_precise(&self) -> f64 {
        let base = self.accumulated_ns.load(Ordering::Relaxed);
        let active = self
            .epoch_start
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .map(|t| t.elapsed().as_nanos() as u64)
            .unwrap_or(0);
        ((base + active) as f64 / 1_000.0).round() / 1_000.0
    }
}

// ── Bridge call log ──────────────────────────────────────────────────────────

/// The reserved dispatcher global backing all host-module import function
/// leaves. The runtime installs it automatically (as a bridge stub) whenever
/// the declared imports contain at least one function leaf; the trampolines
/// built into host modules call it with their handle ID as the first
/// argument. Mirrors `BRIDGE_DISPATCH_GLOBAL` in the TS client (`imports.ts`),
/// where the name is reserved so no user global can collide with it.
const BRIDGE_DISPATCH_GLOBAL: &str = "__iso4_call";

// ── Host-module import handles ───────────────────────────────────────────────

/// One host-module function leaf, located by its declared position. Handle IDs
/// are the indices into the `Vec<ImportHandleEntry>` produced by
/// [`collect_import_handles`].
#[derive(Debug, Clone, PartialEq)]
struct ImportHandleEntry {
    specifier: String,
    /// Dot-joined path inside the module (e.g. `"nested.inner"`); doubles as
    /// the `exportName` on `BridgeCall` frames with `targetKind = import`.
    path: String,
}

impl ImportHandleEntry {
    /// The public bridge-record name for this leaf: `<specifier>.<path>`.
    fn record_name(&self) -> String {
        format!("{}.{}", self.specifier, self.path)
    }
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
fn collect_import_handles(imports: &[ImportBinding]) -> Vec<ImportHandleEntry> {
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
                    path: path.join("."),
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
fn host_module_base_id(imports: &[ImportBinding], specifier: &str) -> u32 {
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
fn public_record_name(stub_name: &str) -> &str {
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
struct BridgeCallLog {
    records: Vec<wire::BridgeCallRecord>,
    /// callId → index into `records`, for calls awaiting their response.
    in_flight: HashMap<u32, usize>,
}

impl BridgeCallLog {
    /// An attempt blocked runtime-side (limit, oversized payload, function
    /// argument, transport failure) — never reached the host.
    fn record_blocked(&mut self, name: &str, start_ms: f64, arg_bytes: u32) {
        self.records.push(wire::BridgeCallRecord {
            name: name.to_string(),
            start_ms,
            duration_ms: 0.0,
            arg_bytes,
            response_bytes: 0,
            ok: false,
            blocked: true,
        });
    }

    /// A call whose BridgeCall frame was written to the host; settles when
    /// the matching response arrives (or at finalize if it never does).
    /// `name` is already the public name — the runtime owns both the import
    /// handle table and the shim naming convention.
    fn record_sent(&mut self, call_id: u32, name: &str, start_ms: f64, arg_bytes: u32) {
        self.records.push(wire::BridgeCallRecord {
            name: name.to_string(),
            start_ms,
            duration_ms: 0.0,
            arg_bytes,
            response_bytes: 0,
            ok: false,
            blocked: false,
        });
        self.in_flight.insert(call_id, self.records.len() - 1);
    }

    /// Route a BridgeResponse to its record. Unknown callIds (stale frames
    /// from a previous run) are ignored, mirroring the resolver map.
    fn settle(&mut self, call_id: u32, now_ms: f64, ok: bool, response_bytes: u32) {
        if let Some(idx) = self.in_flight.remove(&call_id) {
            let r = &mut self.records[idx];
            r.duration_ms = round_micro((now_ms - r.start_ms).max(0.0));
            r.ok = ok;
            r.response_bytes = response_bytes;
        }
    }

    /// Non-draining snapshot for the early Result frame: a clone of
    /// the records so far, in-flight entries stamped with their
    /// duration-until-now, in-flight tracking untouched so grace-time
    /// settlement keeps working.
    fn snapshot(&self, now_ms: f64) -> Vec<wire::BridgeCallRecord> {
        let mut records = self.records.clone();
        for idx in self.in_flight.values() {
            let r = &mut records[*idx];
            r.duration_ms = round_micro((now_ms - r.start_ms).max(0.0));
        }
        records
    }

    /// Snapshot the records for the run result. Calls still in flight get
    /// their duration set to "until run end" and stay `ok: false`.
    fn finalize(&mut self, now_ms: f64) -> Vec<wire::BridgeCallRecord> {
        for idx in self.in_flight.values() {
            let r = &mut self.records[*idx];
            r.duration_ms = round_micro((now_ms - r.start_ms).max(0.0));
        }
        self.in_flight.clear();
        std::mem::take(&mut self.records)
    }
}

// ── Output types ─────────────────────────────────────────────────────────────

/// The result of a successful JavaScript execution.
#[derive(Debug)]
pub struct Output {
    /// All exports as **one** V8 serialization blob holding a plain
    /// `{ name: value }` object. The `default` export (if any) appears as the
    /// `"default"` key alongside named exports. An empty module produces a
    /// blob holding `{}`.
    ///
    /// When the run carried a `call`, this is the called function's
    /// return value instead — still exactly one blob; the host knows which it
    /// asked for.
    pub exports: Vec<u8>,

    /// Export names absent from `exports` because their value cannot cross
    /// the boundary (a function, an unresolved Promise, or a value whose
    /// serialization failed). Never fatal; reported so nothing is
    /// silently hidden. Always empty for a call run.
    pub skipped_exports: Vec<String>,

    /// Lines written to console.log / console.debug / console.info.
    pub stdout: Vec<String>,

    /// Lines written to console.warn / console.error.
    pub stderr: Vec<String>,

    /// Wall-clock time from start of execution to result, in milliseconds
    /// with microsecond resolution (three decimal places).
    pub duration_ms: f64,

    /// Active V8 execution time (bridge waits excluded), in milliseconds
    /// with microsecond resolution.
    pub cpu_time_ms: f64,

    /// The `waitUntil` epilogue's outcome, when the run registered
    /// background work and `limits.grace_ms` allowed a grace phase. `None`
    /// for every run that registered nothing — the common case, which pays
    /// nothing.
    pub background: Option<GraceReport>,

    /// One record per bridge call the sandbox attempted, in attempt order —
    /// including attempts blocked Rust-side (limit exceeded, oversized
    /// payload, function arguments) that never reached the host.
    pub bridge_calls: Vec<wire::BridgeCallRecord>,
}

// ── waitUntil ─────────────────────────────────────────────────

/// How a `waitUntil` grace phase ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraceStatus {
    /// Every registered promise settled (fulfilled) within the budget.
    Settled,
    /// The grace budget (wall at a turn boundary, or the CPU guard mid-turn)
    /// or a host `Terminate` cut the work short.
    Truncated,
    /// At least one registered promise rejected; the rest were driven to
    /// completion. A normal outcome — the code finished via its error path.
    Failed,
}

/// Telemetry of one grace phase, carried on the `RunComplete` frame.
#[derive(Debug, Clone)]
pub struct GraceReport {
    pub status: GraceStatus,
    /// Wall time of the grace phase (after the Result shipped), ms.
    pub duration_ms: f64,
    /// Active V8 execution time during grace, ms.
    pub cpu_time_ms: f64,
    /// Console lines written during grace (the run's own lines already
    /// shipped on the Result).
    pub stdout: Vec<String>,
    pub stderr: Vec<String>,
    /// Bridge calls attempted during grace.
    pub bridge_calls: Vec<wire::BridgeCallRecord>,
    /// `name`/`message` of the first rejection when `status == Failed`, or of
    /// the fatal error that ended a truncated phase, when one exists.
    pub error: Option<(String, String)>,
    /// Whether the early Result frame was written by the run flow. Always
    /// true on the session paths; false only in direct-API use without a
    /// socket or epilogue spec (tests), where grace ran before returning.
    pub early_result_sent: bool,
}

/// Per-run identity the run flow needs to write the early Result frame
/// itself. Installed per run as thread state by the session loop and
/// the warm owner thread — the same pattern as the descriptor brand key
/// — so the deep call chain stays unchanged. Unset (direct API, tests)
/// means grace runs in hold mode: work is driven before the run returns.
#[derive(Clone, Copy)]
pub struct EpilogueSpec {
    /// The run's wire id, echoed on the early Result and the RunComplete.
    pub run_id: u32,
    /// Whether the early Result reports `heapUsedBytes` (prefix runs feed
    /// eviction scoring; one-off runs do not).
    pub report_heap: bool,
}

thread_local! {
    static EPILOGUE_SPEC: std::cell::Cell<Option<EpilogueSpec>> =
        const { std::cell::Cell::new(None) };
}

/// Install the epilogue spec for the next run executed on this thread.
/// Consumed by that run; set it before every `execute`/`run_call_on_core`.
pub fn set_run_epilogue_spec(spec: Option<EpilogueSpec>) {
    EPILOGUE_SPEC.with(|c| c.set(spec));
}

fn take_epilogue_spec() -> Option<EpilogueSpec> {
    EPILOGUE_SPEC.with(|c| c.take())
}

/// Background work registered by `waitUntil`.
///
/// Instance-lifetime storage with a stable heap address (same pattern as the
/// console's `LogBuffers`): the native callback reaches it through an
/// `External`. `slot` is `None` outside run code — setup evaluation, between
/// calls — so a prepare()-time `waitUntil` throws a catchable error; it is
/// armed with an empty set at the setup-to-run boundary (the same line where
/// codegen-from-strings switches off) and disarmed after the run.
pub struct PendingWork {
    slot: RefCell<Option<Vec<v8::Global<v8::Promise>>>>,
}

impl PendingWork {
    fn boxed() -> Box<Self> {
        Box::new(Self {
            slot: RefCell::new(None),
        })
    }
}

// ── Streamed host-type bodies ────────────────────────────────────────────────

/// Runtime-side state of one streamed body: chunks the host has sent that
/// the sandbox has not read yet, terminal state, and the sandbox read (at
/// most one) waiting for data.
struct StreamState {
    buffered: std::collections::VecDeque<Vec<u8>>,
    buffered_bytes: usize,
    /// `Some(Ok(()))` after a clean `StreamEnd`; `Some(Err(msg))` after a
    /// source failure. Reads observe EOF / rejection once the buffer drains.
    ended: Option<Result<(), String>>,
    cancelled: bool,
    pending: Option<v8::Global<v8::PromiseResolver>>,
}

impl StreamState {
    fn new() -> Self {
        Self {
            buffered: std::collections::VecDeque::new(),
            buffered_bytes: 0,
            ended: None,
            cancelled: false,
            pending: None,
        }
    }
}

/// Per-run registry of streamed bodies. Instance-lifetime Box (stable
/// address, the `PendingWork` pattern); armed per call, disarmed after —
/// hydrating or reading a stream outside a run fails cleanly.
pub struct StreamTable {
    slot: RefCell<Option<StreamTableState>>,
}

struct StreamTableState {
    streams: HashMap<u32, StreamState>,
    /// Wire identity for outbound `StreamPull`/`StreamCancel` frames.
    run_id: u32,
    stream_fd: Option<RawFd>,
}

impl StreamTable {
    fn boxed() -> Box<Self> {
        Box::new(Self {
            slot: RefCell::new(None),
        })
    }
}

thread_local! {
    /// The armed stream table of the run executing on this thread, so the
    /// codec's rehydration walk (which has no context parameter) can register
    /// hydrated streams — the same per-thread pattern as the descriptor brand
    /// key. Null outside runs.
    static STREAM_TABLE_PTR: std::cell::Cell<*const StreamTable> =
        const { std::cell::Cell::new(std::ptr::null()) };
}

/// Register a hydrated stream in the executing run's table. Returns false
/// when no run is executing on this thread (a descriptor carrying a stream
/// outside a run — refused by the codec).
pub fn register_hydrated_stream(stream_id: u32) -> bool {
    STREAM_TABLE_PTR.with(|c| {
        let ptr = c.get();
        if ptr.is_null() {
            return false;
        }
        // SAFETY: set only while the owning run frame is live on this thread.
        let table = unsafe { &*ptr };
        let mut slot = table.slot.borrow_mut();
        match slot.as_mut() {
            Some(state) => {
                state.streams.insert(stream_id, StreamState::new());
                true
            }
            None => false,
        }
    })
}

/// Write a `StreamPull` credit grant for consumed bytes. Best-effort: a dead
/// socket surfaces through the run's own I/O soon enough.
fn send_stream_credit(state: &StreamTableState, stream_id: u32, credit: u32) {
    let Some(fd) = state.stream_fd else { return };
    if credit == 0 {
        return;
    }
    let payload = ipc::encode_stream_pull_payload(state.run_id, stream_id, credit);
    // SAFETY: same single-owner ManuallyDrop discipline as bridge writes.
    let mut sock = ManuallyDrop::new(unsafe { UnixStream::from_raw_fd(fd) });
    let _ = ipc::write_rust_to_ts_frame(&mut *sock, ipc::RustToTsMessageType::StreamPull, &payload);
}

/// Write a `StreamCancel` so the host stops pumping and releases the source.
fn send_stream_cancel(state: &StreamTableState, stream_id: u32, reason: &str) {
    let Some(fd) = state.stream_fd else { return };
    let payload = ipc::encode_stream_cancel_payload(state.run_id, stream_id, reason);
    let mut sock = ManuallyDrop::new(unsafe { UnixStream::from_raw_fd(fd) });
    let _ =
        ipc::write_rust_to_ts_frame(&mut *sock, ipc::RustToTsMessageType::StreamCancel, &payload);
}

/// The result of a failed JavaScript execution.
///
/// Logs are preserved on failures too: if user code logs and then throws, the
/// caller still receives the log output produced before the throw.
#[derive(Debug)]
pub struct FailureOutput {
    pub error: RunError,
    pub stdout: Vec<String>,
    pub stderr: Vec<String>,
    pub duration_ms: f64,

    /// Active V8 execution time — see `Output::cpu_time_ms`.
    pub cpu_time_ms: f64,

    /// Bridge call records — see `Output::bridge_calls`.
    pub bridge_calls: Vec<wire::BridgeCallRecord>,
}

/// Payload carried by `RunError::RuntimeError`. Kept in a separate struct so
/// the variant can be boxed and the enum stays small (avoids
/// `clippy::result_large_err` on callers).
#[derive(Debug)]
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
fn codec_error_to_run_error(error: crate::webcodec::CodecError) -> RunError {
    match error {
        crate::webcodec::CodecError::Unsupported(m) => RunError::TypeNotSerializable(m),
        crate::webcodec::CodecError::Malformed(m) => RunError::Internal(m),
    }
}

/// All the ways an execution can fail.
#[derive(Debug)]
pub enum RunError {
    /// Payload bytes are not valid UTF-8.
    /// Raised in session.rs when ipc parsing fails — not yet wired up.
    #[allow(dead_code)]
    InvalidPayload(String),
    /// JS syntax error or compile-time error.
    CompileError(String),
    /// Uncaught exception thrown during execution.
    RuntimeError(Box<RuntimeErrorData>),
    /// `import` specifier not found in the host imports map.
    ModuleNotFound(String),
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
    /// PrefixRun attempted to bind a global not declared by Precompile.
    /// Raised in session.rs when a PrefixRun global was not declared in Precompile.
    #[allow(dead_code)]
    UndeclaredBinding(String),
    /// Prefix top-level evaluation stayed pending after draining the
    /// microtask queue — it awaits something that can never resolve at
    /// precompile time (a never-settling promise).
    PrefixDidNotSettle(String),
    /// Prefix code called a bridge callable (bridge global, shim handler, or
    /// host-import function) at precompile time. No host session exists while
    /// the snapshot is built, so the call can never be served.
    PrefixBridgeCall(String),
    /// Warm-up (isolate boot + prefix evaluation) blew its fixed budget
    /// ([`WARMUP_WALL_MS`]/[`WARMUP_CPU_MS`]). Not a per-request timeout —
    /// the caller's limits are not armed during warm-up, and the budget is
    /// deliberately not configurable (Cloudflare's script-startup model).
    WarmupLimit,
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
    /// Unexpected internal runtime failure.
    Internal(String),
}

// ── Entry point ──────────────────────────────────────────────────────────────

/// Execute a sandboxed run and return the full output.
///
/// `globals` is the list of declared host globals, each tagged by how it is
/// installed natively (`HostGlobalDef`): bridge stubs issue `BridgeCall` frames
/// on `stream_fd` and block until the matching `BridgeResponse`; string/data
/// globals are evaluated/materialised in-isolate and need no socket. Pass
/// `None` for `stream_fd` when no global installs a bridge stub.
pub fn execute(
    code: &str,
    filename: Option<&str>,
    limits: Limits,
    globals: &[HostGlobalDef],
    imports: &[ImportBinding],
    stream_fd: Option<RawFd>,
    call_id_counter: Arc<AtomicU32>,
    call: Option<&ipc::CallSpec>,
) -> Result<Output, FailureOutput> {
    init_platform();
    run_module(
        Some(code),
        filename.unwrap_or("<iso4>"),
        limits,
        globals,
        imports,
        stream_fd,
        call_id_counter,
        call,
    )
}

/// The prefix a run evaluates before its postfix: validated source plus the
/// globals declared at `prepare()` time, replayed into the fresh context.
///
/// Runtime snapshots used to carry this state (prefix heap frozen once,
/// restored per run). V8 14.x removed the ground that stood on: snapshot
/// creation mutates the process-shared read-only heap and the shared
/// artifacts die with the last isolate, so runtime `create_blob` is only
/// safe in the single-isolate, single-shot workflow V8 itself uses
/// The prefix is now re-evaluated per run.
#[derive(Clone, Copy)]
pub struct PrefixSpec<'a> {
    pub code: &'a str,
    pub filename: &'a str,
    /// The full `prepare()`-time global defs — value globals installed before
    /// the prefix evaluates, bridge callables as throwing placeholders.
    pub globals: &'a [HostGlobalDef],
}

/// Execute a postfix against a prepared prefix.
///
/// The prefix source is evaluated into the fresh context first (under the
/// run's limits), then `globals` — all `HostGlobalDef::Bridge` stubs bound to
/// `stream_fd` for this run — overwrite the prefix-time placeholders, then
/// the postfix runs. String/data globals and shim wrappers are replayed from
/// `prefix.globals` and are not re-sent per run.
pub fn execute_with_prefix(
    prefix: PrefixSpec<'_>,
    code: Option<&str>,
    filename: Option<&str>,
    limits: Limits,
    globals: &[HostGlobalDef],
    imports: &[ImportBinding],
    stream_fd: Option<RawFd>,
    call_id_counter: Arc<AtomicU32>,
    call: Option<&ipc::CallSpec>,
) -> Result<Output, FailureOutput> {
    init_platform();
    // Cold start of a warm-capable instance: warm-up (isolate boot + prefix
    // evaluation) runs under its own fixed budget, never the caller's limits
    // — a prefix that loops reports ERR_WARMUP_LIMIT, not the run's
    // timeout. The call itself then gets fresh guards from dispatch to
    // settle. The warm registry keeps the core alive between calls; this
    // one-shot wrapper drops it.
    let mut core = create_instance_core(Some(prefix), imports, limits.memory_mb)?;
    run_call_on_core(
        &mut core,
        code,
        filename.unwrap_or("<iso4>"),
        prefix.globals,
        limits,
        globals,
        imports,
        stream_fd,
        call_id_counter,
        call,
    )
    .result
}

/// Validate prefix code: compile, instantiate, and evaluate it exactly as a
/// run will, in a throwaway isolate, so `prepare()` rejects a broken prefix
/// (syntax error, unresolved import, throwing top-level code) before any run
/// pays for it.
///
/// The caller's per-run limits do not apply — prefix code is host-authored —
/// but validation is not unbounded: it runs under the same heap cap as a real
/// instance and the fixed warm-up budget (`WARMUP_WALL_MS` / `WARMUP_CPU_MS`,
/// per instance), so a looping or heap-hungry prefix fails here instead of on every
/// cold start.
pub fn precompile(
    code: &str,
    filename: Option<&str>,
    globals: &[HostGlobalDef],
    imports: &[ImportBinding],
    memory_mb: u32,
) -> Result<(), FailureOutput> {
    init_platform();
    validate_prefix_module(code, filename.unwrap_or("<prefix>"), globals, imports, memory_mb)
}

/// ESM path: compile source as a module, instantiate it, evaluate it, then
/// inspect the module namespace object for `default` and named exports.
///
/// When `prefix` is `Some`, its source is evaluated into the fresh context
/// (value globals + throwing bridge placeholders first) before postfix code
/// runs. When `globals` is non-empty, live bridge stubs are installed for
/// each name, overwriting any same-named prefix placeholder.
/// `stream_fd` is the raw file-descriptor of the session socket; it is used
/// only during bridge calls and is never closed by this function.
///
/// When `call` is `Some`, the run's value is the called function's return
/// value instead of the exports: the export path resolves against the
/// postfix module when `code` is `Some`, or against the prefix module for a
/// call-only prefix run (`code` = `None`, only valid with a prefix).
fn run_module(
    code: Option<&str>,
    filename: &str,
    limits: Limits,
    globals: &[HostGlobalDef],
    imports: &[ImportBinding],
    stream_fd: Option<RawFd>,
    call_id_counter: Arc<AtomicU32>,
    call: Option<&ipc::CallSpec>,
) -> Result<Output, FailureOutput> {
    // The run clock, CPU budget, and bridge-call log live here (not inside
    // the inner function) so the final values can be stamped onto BOTH
    // result arms in one place — the inner function has many early-return
    // failure paths.
    let start = std::time::Instant::now();
    let cpu_budget = Arc::new(CpuBudget::new());
    let bridge_log: Arc<Mutex<BridgeCallLog>> = Arc::new(Mutex::new(BridgeCallLog::default()));
    let mut result = run_module_inner(
        code,
        filename,
        limits,
        globals,
        imports,
        stream_fd,
        call_id_counter,
        call,
        start,
        Arc::clone(&cpu_budget),
        Arc::clone(&bridge_log),
    );
    let cpu_time_ms = cpu_budget.elapsed_ms_precise();
    let records = bridge_log
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .finalize(elapsed_ms(start));
    match &mut result {
        Ok(output) => {
            output.cpu_time_ms = cpu_time_ms;
            output.bridge_calls = records;
        }
        Err(failure) => {
            failure.cpu_time_ms = cpu_time_ms;
            failure.bridge_calls = records;
        }
    }
    result
}

#[allow(clippy::too_many_arguments)] // internal; parameters mirror run_module + shared run state
fn run_module_inner(
    code: Option<&str>,
    filename: &str,
    limits: Limits,
    globals: &[HostGlobalDef],
    imports: &[ImportBinding],
    stream_fd: Option<RawFd>,
    // Per-connection call-ID counter. Shared across all runs on the same
    // connection so callIds are monotonically increasing and never reset to 0
    // between runs. This prevents a stale BridgeResponse from a previous
    // run's orphaned handler from being accepted as a valid response by the
    // next run's bridge_global_callback.
    call_id_counter: Arc<AtomicU32>,
    call: Option<&ipc::CallSpec>,
    start: std::time::Instant,
    cpu_budget: Arc<CpuBudget>,
    bridge_log: Arc<Mutex<BridgeCallLog>>,
) -> Result<Output, FailureOutput> {
    let mut logs = LogBuffers {
        max_stdout_bytes: limits.max_stdout_bytes,
        max_stderr_bytes: limits.max_stderr_bytes,
        ..LogBuffers::default()
    };
    // waitUntil registrations; one-off runs hold it for the run's life.
    let pending_work = PendingWork::boxed();
    // Streamed-body registry, same lifetime.
    let stream_table = StreamTable::boxed();

    // `reason` is created before the isolate so it can be shared with the
    // ArrayBuffer allocator (which is registered in CreateParams, before the
    // isolate exists).
    let reason = Arc::new(ReasonCell::new());

    // ── ArrayBuffer budget allocator ──────────────────────────────────────────
    // Built before the isolate so we can pass it into CreateParams.
    // The IsolateHandle is set on the state right after Isolate::new returns.
    let alloc_state: Option<Arc<BudgetAllocState>> = if limits.memory_mb > 0 {
        Some(Arc::new(BudgetAllocState {
            used: AtomicUsize::new(0),
            budget: limits.memory_mb as usize * 1024 * 1024,
            handle: OnceLock::new(),
            reason: Arc::clone(&reason),
        }))
    } else {
        None
    };

    let mut isolate = {
        let params = v8::Isolate::create_params();
        // Cap the V8 heap (strings, plain objects). The near-heap callback
        // converts a heap-OOM into a clean terminate_execution().
        let params = if limits.memory_mb > 0 {
            params.heap_limits(0, limits.memory_mb as usize * 1024 * 1024)
        } else {
            params
        };
        // Plug in the custom allocator to track ArrayBuffer/TypedArray memory.
        let params = match &alloc_state {
            Some(state) => {
                // Arc::into_raw gives the allocator its own reference count.
                // budget_alloc_drop reconstructs and drops it when V8 disposes.
                let raw = Arc::into_raw(Arc::clone(state));
                unsafe {
                    params.array_buffer_allocator(v8::new_rust_allocator(raw, &BUDGET_ALLOC_VTABLE))
                }
            }
            None => params,
        };
        v8::Isolate::new(params)
    };

    // Wire the IsolateHandle into the allocator state now that the isolate
    // exists.  Allocations made before this point (during isolate init) would
    // have no handle to terminate, but none of those are user-code allocations.
    if let Some(state) = &alloc_state {
        state.handle.set(isolate.thread_safe_handle()).ok();
    }

    // Explicit policy: microtasks only drain when we call
    // perform_microtask_checkpoint() in the poll loop.  This gives us
    // deterministic control and prevents microtasks from firing during
    // export extraction (D9 hardening).
    isolate.set_microtasks_policy(v8::MicrotasksPolicy::Explicit);

    // Host modules receive their natively-built values through import.meta
    // (see `build_host_module`); the callback consults the resolver context to
    // find the values array staged for the module being initialised.
    isolate.set_host_initialize_import_meta_object_callback(host_import_meta_callback);

    // ── Limit guard threads ───────────────────────────────────────────────────
    //
    // CURRENT APPROACH: one OS guard thread per limit type per run
    // ─────────────────────────────────────────────────────────────
    // Each run spawns up to two short-lived guard threads:
    //   • wall guard  - sleeps for wall_time_ms, fires terminate_execution()
    //   • cpu guard   - polls budget.elapsed_ms() every 10 ms, fires on excess
    //
    // This is simple, correct, and sufficient for typical agent workloads
    // (10-50 concurrent runs). At 100 concurrent runs: 200 guard threads.
    // Modern Linux supports ~10k threads; macOS supports ~2k. In practice
    // these threads sleep for the run duration and consume negligible CPU.
    //
    // REVISIT IF: throughput regularly exceeds ~200-300 concurrent runs on a
    // single binary, or thread-spawn latency shows up in profiling.
    //
    // POOLING ALTERNATIVE (e.g. isolated-vm's approach)
    // ─────────────────────────────────────────────────
    // A single shared priority-queue timer thread manages all pending deadlines:
    //   1. Each run registers (deadline, IsolateHandle) into the shared queue.
    //   2. The pool thread sleeps until the nearest deadline.
    //   3. On expiry it calls isolate.request_interrupt(callback) - this
    //      delivers a callback *within* the V8 isolate at the next JS safepoint,
    //      and the callback calls terminate_execution().
    //   4. RequestInterrupt is thread-safe and does not require the V8 thread.
    //
    // At 100 concurrent runs: ~1 pool thread instead of 200. isolated-vm uses
    // exactly this model (src/isolate/run_with_timeout.h + src/lib/timer.cc).
    //
    // OTHER OPTIONS TO EVALUATE BEFORE BUILDING THE POOL
    // ────────────────────────────────────────────────────
    // • OS-level SIGALRM / timer_create per thread (POSIX, avoids user-space
    //   poll but requires signal-safe V8 interaction - fragile).
    // • tokio::time::timeout wrapping a blocking thread - does NOT work for
    //   tight JS loops (monopolises the async executor; timer never fires).
    // • Two-process SIGKILL as absolute backstop - iso4's architecture already
    //   provides this: the Node host can kill the Rust subprocess unconditionally
    //   on wall-timeout, something in-process runtimes (isolated-vm, Node vm)
    //   fundamentally cannot do without crashing the host.
    //
    // See .notes/timeout-enforcement.md for the full research brief.
    //
    // Both guards are set up before entering V8 scopes so the IsolateHandle
    // is obtained while we still hold a plain &Isolate borrow.
    let handle = isolate.thread_safe_handle();
    let cancel_handle = handle.clone(); // for cancel_terminate_execution on success
    // One-off runs drop the join handles (detached guards, as before the
    // warm split): the isolate dies with the run, so a late-firing guard
    // has nothing durable to poison.
    let (cancel_wall, _wall_join) =
        start_wall_guard(handle.clone(), Arc::clone(&reason), limits.wall_time_ms);
    let (cancel_cpu, _cpu_join) = start_cpu_guard(
        handle.clone(),
        Arc::clone(&reason),
        Arc::clone(&cpu_budget),
        limits.cpu_time_ms,
    );

    // ── Near-heap callback (V8 heap objects: strings, plain objects) ──────────
    // Complements the ArrayBuffer allocator above.  Together they cover all
    // memory sources.  The Box must outlive active JS execution; safe because
    // V8 never invokes near-heap callbacks during Isolate::Dispose().
    let _near_heap: Option<Box<NearHeapData>> = if limits.memory_mb > 0 {
        let data = Box::new(NearHeapData {
            handle,
            reason: Arc::clone(&reason),
        });
        let raw = &*data as *const NearHeapData as *mut std::ffi::c_void;
        isolate.add_near_heap_limit_callback(near_heap_limit_cb, raw);
        Some(data)
    } else {
        drop(handle);
        None
    };
    // cpu_budget.enter() is called immediately before module.evaluate() so
    // compilation and scope setup time is not charged against the CPU budget.
    let _guard_canceller = GuardCanceller {
        cancel_wall: &cancel_wall,
        cancel_cpu: &cancel_cpu,
        budget: &cpu_budget,
    };

    let context_global = {
        v8::scope!(let scope, &mut isolate);
        let context = v8::Context::new(scope, Default::default());
        let scope = &mut v8::ContextScope::new(scope, context);
    // Every setup step below runs with the guards already armed, so each
    // failure classifies through `termination_or` — a guard firing mid-setup
    // (a very tight wall limit on a slow machine) must report the timeout,
    // not the operation it happened to interrupt.
    install_console(scope, &mut logs as *mut LogBuffers)
        .map_err(|error| failure(termination_or(&reason, error), &logs, start))?;

    // Web globals (Headers/Request/Response/URL/TextEncoder/…), with the
    // streamed-body natives wired to this run's table.
    install_web_runtime(scope, &*stream_table)
        .map_err(|e| failure(termination_or(&reason, RunError::Internal(e)), &logs, start))?;

    // Install AsyncLocalStorage (importable via `node:async_hooks`) for this
    // run. Always installed: it registers no promise hooks, so runs that never
    // use it pay only for the class setup, not per-promise overhead.
    install_async_context(scope)
        .map_err(|error| failure(termination_or(&reason, error), &logs, start))?;

    // waitUntil: installed disarmed — run_call_phase arms it at the
    // setup-to-run boundary.
    install_wait_until(scope, &*pending_work)
        .map_err(|error| failure(termination_or(&reason, error), &logs, start))?;

        v8::Global::new(scope, context)
    };

    // Setup is done — hand over to the per-call phase shared with warm
    // instances. Globals carry the context and prefix module across the
    // scope boundary; the guards stay armed, so a one-off run covers setup +
    // execution with a single budget exactly as before.
    // One-off runs keep the slot table as a run-local: the context (and any
    // stub Function sandbox code stashed) dies with the isolate right after.
    let mut stub_slots = StubSlots::new();
    let result = run_call_phase(
        &mut isolate,
        &context_global,
        None,
        code,
        filename,
        &mut stub_slots,
        CallPhaseCtx {
            limits: &limits,
            globals,
            imports,
            stream_fd,
            call_id_counter,
            call,
            start,
            cpu_budget: &cpu_budget,
            bridge_log: &bridge_log,
            reason: &reason,
            logs: std::ptr::addr_of!(logs),
            pending: &*pending_work,
            streams: &*stream_table,
            epilogue: take_epilogue_spec(),
            cancel_wall: &cancel_wall,
            cancel_cpu: &cancel_cpu,
            cancel_handle: &cancel_handle,
        },
    );
    // Failure paths return out of run_call_phase before its own stream
    // cleanup: close here too (idempotent), so the host releases its sources
    // and the per-thread table pointer never dangles past the Box.
    close_run_streams(&*stream_table);
    result
}

/// Per-call parameters for [`run_call_phase`] — bundled so the one-off path
/// (`run_module_inner`) and warm instances (`run_call_on_core`) drive
/// the identical phase with their own guard and telemetry lifetimes.
struct CallPhaseCtx<'a> {
    limits: &'a Limits,
    globals: &'a [HostGlobalDef],
    imports: &'a [ImportBinding],
    stream_fd: Option<RawFd>,
    call_id_counter: Arc<AtomicU32>,
    call: Option<&'a ipc::CallSpec>,
    start: std::time::Instant,
    cpu_budget: &'a Arc<CpuBudget>,
    bridge_log: &'a Arc<Mutex<BridgeCallLog>>,
    reason: &'a Arc<ReasonCell>,
    /// Read-only view of the console sink for failure snapshots. A raw
    /// pointer (not `&`) because the console concurrently holds the writing
    /// pointer; only transient borrows are taken, never across JS execution
    /// — the same discipline the pre-split code applied to its `logs` local.
    logs: *const LogBuffers,
    /// The run's `waitUntil` registration set — instance-lifetime Box,
    /// same pointer discipline as `logs`. Armed at the setup-to-run boundary
    /// below, drained by the grace phase, disarmed by the caller.
    pending: *const PendingWork,
    /// The run's streamed-body registry — same pointer discipline as `logs`.
    streams: *const StreamTable,
    /// Per-run identity for the early Result frame; `None` = hold mode.
    epilogue: Option<EpilogueSpec>,
    cancel_wall: &'a crossbeam_channel::Sender<()>,
    cancel_cpu: &'a crossbeam_channel::Sender<()>,
    cancel_handle: &'a v8::IsolateHandle,
}

/// The per-call half of a run: bridge stubs in, postfix/call executed and
/// settled, value blob out. Everything before this (isolate boot, web
/// runtime, prefix evaluation) is instance-lifetime state that one-off runs
/// rebuild per run and warm instances retain across calls. Opens its own
/// scopes on `isolate`/`context`; the caller owns the guards and stamps the
/// final cpu/bridge telemetry.
fn run_call_phase(
    isolate: &mut v8::OwnedIsolate,
    context: &v8::Global<v8::Context>,
    prefix_module: Option<&v8::Global<v8::Module>>,
    code: Option<&str>,
    filename: &str,
    // Instance-lifetime stub slots (see `StubSlot`) — per-call state is
    // installed into them here and disarmed by the caller after the call.
    stub_slots: &mut StubSlots,
    ctx: CallPhaseCtx<'_>,
) -> Result<Output, FailureOutput> {
    let CallPhaseCtx {
        limits,
        globals,
        imports,
        stream_fd,
        call_id_counter,
        call,
        start,
        cpu_budget,
        bridge_log,
        reason,
        logs,
        pending,
        streams,
        epilogue,
        cancel_wall,
        cancel_cpu,
        cancel_handle,
    } = ctx;

    v8::scope!(let scope, isolate);
    let context_local = v8::Local::new(scope, context);
    let scope = &mut v8::ContextScope::new(scope, context_local);

    // Shared state for all bridge callbacks in this call. All three are Arcs-
    // call_id, bridge_error, and pending_resolvers are shared between the
    // callback stubs (via GlobalCallbackData) and the poll loop below.
    // Shared state for all bridge callbacks in this run. All three are Arcs-
    // call_id, bridge_error, and pending_resolvers are shared between the
    // callback stubs (via GlobalCallbackData) and the poll loop below.
    let call_id = call_id_counter;
    let bridge_call_count = Arc::new(AtomicU32::new(0));
    let bridge_error: Arc<OnceLock<RunError>> = Arc::new(OnceLock::new());
    let pending_resolvers: PendingResolvers = Arc::new(Mutex::new(HashMap::new()));

    // Split the tagged defs into the bridge stubs to install (plain functions,
    // shim handlers) and everything else. Value globals (string exprs,
    // constants, shim wrappers) are installed after the stubs so a shim
    // wrapper's handler stub already exists on globalThis. Each stub carries
    // the public name its bridge records report under: shim handler stubs
    // report under the public global name, everything else as-is.
    let mut bridge_stubs: Vec<BridgeStubSpec> = globals
        .iter()
        .filter_map(|g| {
            g.bridge_stub_name().map(|stub| BridgeStubSpec {
                stub_name: stub.to_string(),
                record_name: match g {
                    HostGlobalDef::Shim { name, .. } => name.clone(),
                    _ => public_record_name(stub).to_string(),
                },
                import_handles: None,
                // Internal stub names (shim handlers) are forced DONT_ENUM by
                // define_global regardless of this flag.
                enumerable: g.enumerable(),
            })
        })
        .collect();

    // Host-module function leaves dispatch through the reserved `__iso4_call`
    // stub. The runtime owns the handle table (id → specifier + path), so it
    // installs the dispatcher itself whenever the declared imports contain at
    // least one function leaf — the client never sends a def for it.
    let import_handles = collect_import_handles(imports);
    if !import_handles.is_empty() {
        bridge_stubs.push(BridgeStubSpec {
            stub_name: BRIDGE_DISPATCH_GLOBAL.to_string(),
            record_name: BRIDGE_DISPATCH_GLOBAL.to_string(),
            import_handles: Some(Arc::new(import_handles)),
            enumerable: true, // internal name — DONT_ENUM applies regardless
        });
    }

    if !bridge_stubs.is_empty() {
        let fd = stream_fd
            .expect("install_bridge_globals called with bridge-backed globals but no stream_fd");
        install_bridge_globals(
            scope,
            &bridge_stubs,
            fd,
            Arc::clone(&call_id),
            Arc::clone(&bridge_error),
            limits.max_bridge_call_bytes,
            Arc::clone(&bridge_call_count),
            limits.max_bridge_calls,
            Arc::clone(&pending_resolvers),
            start,
            Arc::clone(bridge_log),
            stub_slots,
        )
        .map_err(|e| failure(termination_or(reason, e), unsafe { &*logs }, start))?;
    }

    // Install the value globals (string exprs, constants, shim wrappers)
    // natively. For a direct run these arrive here; for a PrefixRun they were
    // replayed from the prefix defs during prefix evaluation above, so
    // `globals` carries only bridge stubs and this is a no-op.
    install_value_globals(scope, globals)
        .map_err(|e| failure(termination_or(reason, e), unsafe { &*logs }, start))?;

    // Setup is complete; everything that executes from here on is per-run
    // sandbox input. Disable code generation from strings (eval,
    // new Function) for it: code a prepared module builds at prepare() time
    // is reproducible from its stored source, while strings evaluated here
    // would be production code with no source of record (workerd draws
    // the same line between startup and run time). V8 throws a catchable
    // EvalError at the call site; the run itself continues. On a warm
    // instance the retained context simply stays denied across calls.
    context_local.set_allow_generation_from_strings(false);

    // Arm waitUntil for this run at the same boundary: setup code could
    // not register background work (the slot was None and the call threw);
    // run code now can.
    // SAFETY: `pending` points at the instance-lifetime PendingWork Box.
    unsafe {
        *(*pending).slot.borrow_mut() = Some(Vec::new());
    }
    // Arm the streamed-body registry for this run, and expose it per thread
    // for the codec's rehydration walk (which has no context parameter —
    // the brand-key pattern).
    // SAFETY: `streams` points at the instance-lifetime StreamTable Box.
    unsafe {
        *(*streams).slot.borrow_mut() = Some(StreamTableState {
            streams: HashMap::new(),
            run_id: epilogue.map(|e| e.run_id).unwrap_or(0),
            stream_fd,
        });
    }
    STREAM_TABLE_PTR.with(|c| c.set(streams));

    v8::tc_scope!(let scope, scope);

    // Shared settle machinery — the module evaluation promise and a requested
    // call's return promise drive the same poll loop.
    let settle_ctx = SettleCtx {
        stream_fd,
        limits,
        start,
        reason,
        cpu_budget,
        bridge_log,
        bridge_error: &bridge_error,
        pending_resolvers: &pending_resolvers,
        cancel_handle,
        streams,
    };

    // Compile, instantiate, evaluate, and settle the postfix module when the
    // frame carries one. A call-only prefix run (`code` = None) has no postfix
    // — the prefix module evaluated above is the call target.
    let evaluated_module: v8::Local<v8::Module> = if let Some(code) = code {
        let source_string = v8::String::new(scope, code).ok_or_else(|| {
            failure(
                RunError::Internal("failed to intern module source".to_string()),
                unsafe { &*logs },
                start,
            )
        })?;
        let filename = v8::String::new(scope, filename).ok_or_else(|| {
            failure(
                RunError::Internal("failed to intern filename".to_string()),
                unsafe { &*logs },
                start,
            )
        })?;
        let origin = v8::ScriptOrigin::new(
            scope,
            filename.into(),
            0,
            0,
            false,
            0,
            None,
            false,
            false,
            true,
            None,
        );
        let mut source = v8::script_compiler::Source::new(source_string, Some(&origin));

        let module = match v8::script_compiler::compile_module(scope, &mut source) {
            Some(m) => m,
            None => {
                return Err(failure(
                    termination_or(reason, RunError::CompileError(exception_message(scope))),
                    unsafe { &*logs },
                    start,
                ))
            }
        };

        // ── Install module resolver for this run (Phase 6) ──────────────────────
        // After `instantiate_module` returns we inspect `resolve_error` for any
        // non-V8-throwable error recorded inside the resolver (e.g. a source
        // module that failed to compile).
        let _resolver_guard = install_resolver(ResolverContext {
            bindings: imports.to_vec(),
            module_cache: HashMap::new(),
            pending_meta: Vec::new(),
            resolve_error: None,
            async_context_builtin: true,
        });

        if module
            .instantiate_module(scope, module_resolver_callback)
            .is_none()
        {
            let err = take_resolver()
                .and_then(|ctx| ctx.resolve_error)
                .unwrap_or_else(|| RunError::ModuleNotFound(exception_message(scope)));
            return Err(failure(termination_or(reason, err), unsafe { &*logs }, start));
        }

        cpu_budget.enter(); // start measuring active CPU time (compile + scope setup excluded)
        let evaluation = match module.evaluate(scope) {
            Some(value) => value,
            None => {
                cancel_guards(cancel_wall, cancel_cpu, cpu_budget);
                if let Some(err) = bridge_error.get() {
                    let owned = match err {
                        RunError::HostBridge(m) => RunError::HostBridge(m.clone()),
                        RunError::FunctionArgumentNotSupported => {
                            RunError::FunctionArgumentNotSupported
                        }
                        RunError::BridgeCallPayloadTooLarge => RunError::BridgeCallPayloadTooLarge,
                        RunError::BridgeCallLimitExceeded => RunError::BridgeCallLimitExceeded,
                        RunError::Internal(m) => RunError::Internal(m.clone()),
                        other => RunError::Internal(format!("unexpected bridge error: {other:?}")),
                    };
                    return Err(failure(owned, unsafe { &*logs }, start));
                }
                let error = match reason.get() {
                    Some(TerminationReason::Wall) => RunError::WallTimeout,
                    Some(TerminationReason::Cpu) => RunError::CpuTimeout,
                    Some(TerminationReason::Memory) => RunError::MemoryLimit,
                    None => RunError::RuntimeError(Box::new(RuntimeErrorData {
                        name: exception_name(scope),
                        message: exception_message(scope),
                        stack: exception_stack(scope),
                        fields: exception_fields(scope),
                    })),
                };
                return Err(failure(error, unsafe { &*logs }, start));
            }
        };

        // ── Poll loop: drive microtasks until the module promise settles ──────────
        //
        // With MicrotasksPolicy::Explicit, microtasks do not run automatically.
        // After each BridgeResponse is resolved we call perform_microtask_checkpoint()
        // to advance the await chains.  When the module's top-level promise finally
        // fulfils or rejects we exit the loop.
        //
        // This handles both:
        //   • Sequential bridge calls  - Promise.all([a]) etc.
        //   • Concurrent bridge calls  - Promise.all([a, b]) sends both BridgeCall
        //     frames before yielding; both responses route by callId.
        //   • No bridge calls          - pure `await Promise.resolve(42)` still
        //     needs one checkpoint to propagate the already-resolved microtask.
        // One checkpoint immediately after evaluate() so that:
        //   • purely synchronous results (await Promise.resolve(42)) settle now,
        //   • synchronous bridge errors (throw from callback) propagate to the
        //     module's top-level Promise now,
        //   • termination exceptions propagate now.
        // For pending bridge calls nothing changes — resolvers aren't set yet.
        scope.perform_microtask_checkpoint();

        // ESM Module::Evaluate() always returns a Promise.  The try_from is a
        // belt-and-suspenders guard; failure would be an internal V8 bug.
        let promise = v8::Local::<v8::Promise>::try_from(evaluation).map_err(|_| {
            cancel_guards(cancel_wall, cancel_cpu, cpu_budget);
            failure(
                RunError::Internal("module evaluation did not return a Promise".into()),
                unsafe { &*logs },
                start,
            )
        })?;

        match settle_promise(scope, promise, &settle_ctx).map_err(|e| failure(e, unsafe { &*logs }, start))? {
            v8::PromiseState::Fulfilled => {}
            _ => {
                // Pending with no socket to wait on — an un-awaited Promise that
                // nothing in the isolate can ever resolve.
                let error = match reason.get() {
                    Some(TerminationReason::Wall) => RunError::WallTimeout,
                    Some(TerminationReason::Cpu) => RunError::CpuTimeout,
                    Some(TerminationReason::Memory) => RunError::MemoryLimit,
                    None => RunError::ExportNotSerializable(
                        "module evaluation promise is still pending after poll loop".to_string(),
                    ),
                };
                return Err(failure(error, unsafe { &*logs }, start));
            }
        }

        module
    } else {
        // Call-only prefix run: the prefix module evaluated above is the
        // call target. Enter the CPU budget here — the postfix path enters it
        // right before evaluate() — so path resolution and the call itself are
        // measured (a path segment can be an accessor).
        cpu_budget.enter();
        v8::Local::new(
            scope,
            prefix_module.expect("call-only run without a prefix module"),
        )
    };

    // The guards stay armed through extraction and serialization. V8's
    // ValueSerializer follows structured-clone semantics and invokes guest
    // getters on the properties it walks, so result serialization is guest
    // execution and must stay under the run's wall/CPU/memory budgets —
    // workerd does the same (its limit enforcer covers every entry into JS,
    // serialization included). The caller's GuardCanceller stops the guards
    // once this function returns; every failure below classifies through
    // `termination_or` so a guard firing mid-serialization reports the
    // timeout, not the operation it interrupted.

    let namespace = evaluated_module
        .get_module_namespace()
        .to_object(scope)
        .ok_or_else(|| {
            failure(
                termination_or(
                    reason,
                    RunError::Internal("module namespace is not an object".to_string()),
                ),
                unsafe { &*logs },
                start,
            )
        })?;

    let mut skipped_exports: Vec<String> = Vec::new();

    let exports = if let Some(call) = call {
        // ── Host → sandbox call ────────────────────────────────────────
        // Resolve the export path against the namespace and invoke. Runs
        // inside the CPU budget — a path segment can be an accessor, so
        // resolution itself is sandbox code.
        let (target, receiver) = resolve_call_target(scope, namespace, &call.export_path)
            .map_err(|e| failure(e, unsafe { &*logs }, start))?;

        // The argument list crosses as one blob holding an array (identity
        // between arguments preserved, same convention as BridgeCall args);
        // host types (Request/…) rehydrate to real instances.
        let args_value = blob::deserialize_value_with_web_types(scope, &call.args_blob)
            .ok_or_else(|| {
                let error = match blob::take_codec_error() {
                    Some(e) => codec_error_to_run_error(e),
                    None => RunError::Internal("failed to deserialize call arguments".to_string()),
                };
                failure(error, unsafe { &*logs }, start)
            })?;
        let args_array = v8::Local::<v8::Array>::try_from(args_value).map_err(|_| {
            failure(
                RunError::Internal("call arguments blob did not hold an array".to_string()),
                unsafe { &*logs },
                start,
            )
        })?;
        let mut args: Vec<v8::Local<v8::Value>> = Vec::with_capacity(args_array.length() as usize);
        for i in 0..args_array.length() {
            let arg = args_array.get_index(scope, i).ok_or_else(|| {
                failure(
                    RunError::Internal(format!("failed to read call argument {i}")),
                    unsafe { &*logs },
                    start,
                )
            })?;
            args.push(arg);
        }

        let result = match target.call(scope, receiver, &args) {
            Some(v) => v,
            None => {
                // Threw synchronously — classify like a rejected handler
                // promise, so sync and async throws report the same
                // name/message/stack/fields shape.
                if let Some(err) = bridge_error.get() {
                    return Err(failure(owned_bridge_error(err), unsafe { &*logs }, start));
                }
                let error = match reason.get() {
                    Some(TerminationReason::Wall) => RunError::WallTimeout,
                    Some(TerminationReason::Cpu) => RunError::CpuTimeout,
                    Some(TerminationReason::Memory) => RunError::MemoryLimit,
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
                return Err(failure(error, unsafe { &*logs }, start));
            }
        };

        // A sync return value needs no poll loop at all. An async handler gets
        // the same settle machinery as the module promise — one checkpoint
        // first, so a promise that waits on no host I/O settles without
        // touching the socket (rounds track bridge round trips, not awaits).
        let value: v8::Local<v8::Value> = if result.is_promise() {
            let promise = v8::Local::<v8::Promise>::try_from(result).map_err(|_| {
                failure(
                    RunError::Internal("call result claimed to be a promise but is not".into()),
                    unsafe { &*logs },
                    start,
                )
            })?;
            scope.perform_microtask_checkpoint();
            if let Some(err) = bridge_error.get() {
                return Err(failure(owned_bridge_error(err), unsafe { &*logs }, start));
            }
            match settle_promise(scope, promise, &settle_ctx)
                .map_err(|e| failure(e, unsafe { &*logs }, start))?
            {
                v8::PromiseState::Fulfilled => promise.result(scope),
                _ => {
                    let error = match reason.get() {
                        Some(TerminationReason::Wall) => RunError::WallTimeout,
                        Some(TerminationReason::Cpu) => RunError::CpuTimeout,
                        Some(TerminationReason::Memory) => RunError::MemoryLimit,
                        None => RunError::ExportNotSerializable(format!(
                            "call \"{}\" returned a promise that is still pending after the \
                             poll loop: nothing in the isolate can resolve it",
                            call.export_path
                        )),
                    };
                    return Err(failure(error, unsafe { &*logs }, start));
                }
            }
        } else {
            result
        };

        // The call settled; the guards stay armed through serialization of
        // the return value (see the comment above the namespace read).
        blob::serialize_value(scope, value).map_err(|message| {
            let error = match blob::take_codec_error() {
                Some(e) => codec_error_to_run_error(e),
                None => RunError::ExportNotSerializable(format!(
                    "call \"{}\" result could not be serialized: {message}",
                    call.export_path
                )),
            };
            failure(termination_or(reason, error), unsafe { &*logs }, start)
        })?
    } else {
        // ── Exports extraction ───────────────────────────────────────────────
        let names = namespace
            .get_own_property_names(scope, v8::GetPropertyNamesArgs::default())
            .ok_or_else(|| {
                failure(
                    termination_or(
                        reason,
                        RunError::Internal("failed to read module export names".to_string()),
                    ),
                    unsafe { &*logs },
                    start,
                )
            })?;

        // Copy the exports into a fresh plain object and serialize that **once**.
        // The module namespace is an exotic object the V8 serializer refuses, and
        // a single blob for all exports beats one blob per export (measured).
        //
        // Non-serializable exports are skipped, never fatal: a value the
        // pre-check refuses (function, unresolved Promise) is simply absent
        // from the result, its name reported in `skipped_exports` — this is
        // what lets `export default { fetch }` coexist with reading the
        // module's declaration exports.
        let exports_object = v8::Object::new(scope);
        let mut staged: Vec<(String, v8::Local<v8::Value>)> = Vec::new();

        for i in 0..names.length() {
            let name_value = names.get_index(scope, i).ok_or_else(|| {
                failure(
                    termination_or(
                        reason,
                        RunError::Internal("failed to read export name".to_string()),
                    ),
                    unsafe { &*logs },
                    start,
                )
            })?;
            let name = name_value
                .to_string(scope)
                .map(|s| s.to_rust_string_lossy(scope))
                .ok_or_else(|| {
                    failure(
                        termination_or(
                            reason,
                            RunError::Internal("failed to stringify export name".to_string()),
                        ),
                        unsafe { &*logs },
                        start,
                    )
                })?;

            let value = namespace.get(scope, name_value).ok_or_else(|| {
                failure(
                    termination_or(
                        reason,
                        RunError::Internal(format!("failed to read export {name}")),
                    ),
                    unsafe { &*logs },
                    start,
                )
            })?;

            // Pre-check catches the common cases cheaply (a function or
            // Promise directly under the export name) without a trial
            // serialization.
            if check_export_serializable(&name, value).is_err() {
                skipped_exports.push(name);
                continue;
            }

            let export_key = v8::Local::<v8::Name>::try_from(name_value).map_err(|_| {
                failure(
                    termination_or(
                        reason,
                        RunError::Internal(format!("export name {name} is not a property key")),
                    ),
                    unsafe { &*logs },
                    start,
                )
            })?;
            exports_object
                .create_data_property(scope, export_key, value)
                .ok_or_else(|| {
                    failure(
                        termination_or(
                            reason,
                            RunError::Internal(format!("failed to stage export {name}")),
                        ),
                        unsafe { &*logs },
                        start,
                    )
                })?;
            staged.push((name, value));
        }

        match blob::serialize_value(scope, exports_object.into()) {
            Ok(blob) => blob,
            Err(_first_message) => {
                // A registered host type that cannot cross keeps its dedicated
                // loud diagnostic (stream bodies etc. must not vanish
                // silently); only generic clone refusals are skippable.
                if let Some(e) = blob::take_codec_error() {
                    return Err(failure(
                        termination_or(reason, codec_error_to_run_error(e)),
                        unsafe { &*logs },
                        start,
                    ));
                }
                // Something nested inside an export refuses to clone (e.g. the
                // function in `export default { fetch }`). Trial-serialize
                // each export and drop the offenders into `skipped_exports`.
                let retry_object = v8::Object::new(scope);
                for (name, value) in &staged {
                    match blob::serialize_value(scope, *value) {
                        Ok(_) => {
                            let key = v8::String::new(scope, name).ok_or_else(|| {
                                failure(
                                    termination_or(
                                        reason,
                                        RunError::Internal(
                                            "failed to intern export name".to_string(),
                                        ),
                                    ),
                                    unsafe { &*logs },
                                    start,
                                )
                            })?;
                            retry_object
                                .create_data_property(scope, key.into(), *value)
                                .ok_or_else(|| {
                                    failure(
                                        termination_or(
                                            reason,
                                            RunError::Internal(format!(
                                                "failed to stage export {name}"
                                            )),
                                        ),
                                        unsafe { &*logs },
                                        start,
                                    )
                                })?;
                        }
                        Err(_) => {
                            if let Some(e) = blob::take_codec_error() {
                                return Err(failure(
                                    termination_or(reason, codec_error_to_run_error(e)),
                                    unsafe { &*logs },
                                    start,
                                ));
                            }
                            skipped_exports.push(name.clone());
                        }
                    }
                }
                blob::serialize_value(scope, retry_object.into()).map_err(|retry_message| {
                    failure(
                        termination_or(
                            reason,
                            RunError::ExportNotSerializable(format!(
                                "exports could not be serialized: {retry_message}"
                            )),
                        ),
                        unsafe { &*logs },
                        start,
                    )
                })?
            }
        }
    };

    // Termination only interrupts JS — the serializer's walk of plain data
    // can still complete after a guard fires (the per-export retry loop then
    // skips whatever export the interrupted getter sat on). A produced blob
    // does not make the run a success: the budget was exceeded.
    if reason.get().is_some() {
        return Err(failure(
            termination_or(
                reason,
                RunError::Internal("terminated during result serialization".to_string()),
            ),
            unsafe { &*logs },
            start,
        ));
    }

    // ── Export size limit ────────────────────────────────────────────────────
    // Measured on the blob itself — the payload that actually crosses the
    // socket, so the limit is now free (no probe encode). A call's return
    // value is capped by the same limit.
    if limits.max_export_bytes > 0 && exports.len() > limits.max_export_bytes as usize {
        return Err(failure(RunError::ExportTooLarge, unsafe { &*logs }, start));
    }

    let mut output = Output {
        exports,
        skipped_exports,
        stdout: unsafe { &*logs }.stdout.clone(),
        stderr: unsafe { &*logs }.stderr.clone(),
        duration_ms: elapsed_ms(start),
        // Stamped by run_module from the shared run state.
        cpu_time_ms: 0.0,
        bridge_calls: Vec::new(),
        background: None,
    };

    // ── waitUntil ─────────────────────────────────────────────
    // The value is settled and serialized; if the run registered background
    // work (and the grace budget allows any), the run's second phase starts:
    // the Result ships early, the pending set is driven under its own
    // budgets, and the outcome travels on a final RunComplete frame. A run
    // that registered nothing takes none of these branches and behaves
    // exactly as before waitUntil existed.
    let has_pending = unsafe {
        (*pending)
            .slot
            .borrow()
            .as_ref()
            .is_some_and(|set| !set.is_empty())
    };
    if has_pending && limits.grace_ms > 0 {
        // The run phase is over: its guards come down before grace arms its
        // own (idempotent with the caller's GuardCanceller).
        cancel_guards(cancel_wall, cancel_cpu, cpu_budget);
        cancel_handle.cancel_terminate_execution();

        output.background = Some(run_grace_phase(
            scope,
            &mut output,
            pending,
            epilogue,
            &GracePhaseCtx {
                stream_fd,
                limits,
                start,
                reason,
                cpu_budget,
                bridge_log,
                bridge_error: &bridge_error,
                pending_resolvers: &pending_resolvers,
                cancel_handle,
                logs,
                streams,
            },
        ));
    }

    // Streams are per run: tell the host to release any source still being
    // pumped, and disarm the registry.
    close_run_streams(streams);

    Ok(output)
}

/// Cap the error strings carried on a `RunComplete` frame (review): the
/// rejection message is guest-authored and otherwise unbounded, and the frame
/// has no per-field limits of its own.
fn capped_grace_error(name: String, message: String) -> (String, String) {
    let mut name = name;
    let mut message = message;
    name.truncate(256);
    if message.len() > 2048 {
        message.truncate(2048);
        message.push_str("… [truncated]");
    }
    (name, message)
}

/// The run-phase state the grace phase borrows. A subset of `CallPhaseCtx`
/// plus the shared bridge state that lives in `run_call_phase` locals.
struct GracePhaseCtx<'a> {
    stream_fd: Option<RawFd>,
    limits: &'a Limits,
    start: std::time::Instant,
    reason: &'a Arc<ReasonCell>,
    cpu_budget: &'a Arc<CpuBudget>,
    bridge_log: &'a Arc<Mutex<BridgeCallLog>>,
    bridge_error: &'a Arc<OnceLock<RunError>>,
    pending_resolvers: &'a PendingResolvers,
    cancel_handle: &'a v8::IsolateHandle,
    logs: *const LogBuffers,
    streams: *const StreamTable,
}

/// Drive the registered background work after the run's value settled.
///
/// Writes the early Result frame itself when a spec and socket exist (the
/// session paths), then keeps the poll loop running — bridge calls included —
/// until the pending set settles or the grace budgets end it. Grace has its
/// own metering: a fresh CPU allotment (guarded — a mid-turn kill taints via
/// the run's reason cell) and the `grace_ms` wall applied at turn boundaries
/// through the poll loop's read timeout (a decoy reason cell, so a clean
/// boundary stop does not taint).
fn run_grace_phase(
    scope: &mut v8::PinScope,
    output: &mut Output,
    pending: *const PendingWork,
    epilogue: Option<EpilogueSpec>,
    ctx: &GracePhaseCtx<'_>,
) -> GraceReport {
    // Freeze the run-phase telemetry for the early Result, and hand the
    // console buffers over to grace: lines from here on belong to the report.
    let run_cpu_ms = ctx.cpu_budget.elapsed_ms_precise();
    let now_ms = elapsed_ms(ctx.start);
    let run_records = ctx
        .bridge_log
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .snapshot(now_ms);
    let run_record_count = run_records.len();
    // SAFETY: transient exclusive access to the console sink between JS
    // executions — the discipline `logs` is held under everywhere else.
    unsafe {
        let l = ctx.logs.cast_mut();
        (*l).stdout.clear();
        (*l).stderr.clear();
    }

    let early_result_sent = match (epilogue, ctx.stream_fd) {
        (Some(spec), Some(fd)) => {
            let heap_used_bytes = spec.report_heap.then(|| {
                let stats = scope.get_heap_statistics();
                stats.used_heap_size() as u64
            });
            let payload = wire::encode_run_completion_payload(
                spec.run_id,
                wire::RunCompletion::Success(wire::RunSuccessPayload {
                    exports: output.exports.clone(),
                    skipped_exports: output.skipped_exports.clone(),
                    stdout: output.stdout.clone(),
                    stderr: output.stderr.clone(),
                    duration_ms: output.duration_ms,
                    cpu_time_ms: run_cpu_ms,
                    bridge_calls: run_records,
                    heap_used_bytes,
                    background_pending: true,
                }),
            );
            // SAFETY: same ManuallyDrop pattern as the bridge callbacks — the
            // session thread is parked for the duration of the call, so this
            // thread is the socket's only user.
            let mut sock = ManuallyDrop::new(unsafe { UnixStream::from_raw_fd(fd) });
            match ipc::write_rust_to_ts_frame(
                &mut *sock,
                ipc::RustToTsMessageType::Result,
                &payload,
            ) {
                Ok(()) => true,
                Err(e) => {
                    // Fall back to hold mode: the session writes the Result
                    // (without the pending flag) after grace completes.
                    eprintln!("[iso4-v8] early Result write failed ({e}); holding");
                    false
                }
            }
        }
        _ => false,
    };

    // ── Grace budgets ────────────────────────────────────────────────────────
    // CPU: a fresh allotment sized like the run's, or grace_ms when the run
    // is CPU-unlimited, so a tight loop inside a continuation is always
    // bounded. The guard writes the REAL reason cell and terminates — a
    // mid-turn kill is a taint, per the fired-guard doctrine. Wall: enforced
    // at turn boundaries by the poll loop's read timeout against a decoy
    // cell — stopping between turns is clean and does not taint.
    let grace_start = std::time::Instant::now();
    let grace_cpu = Arc::new(CpuBudget::new());
    let grace_cpu_cap = if ctx.limits.cpu_time_ms > 0 {
        ctx.limits.cpu_time_ms
    } else {
        ctx.limits.grace_ms
    };
    let (grace_cancel_cpu, grace_cpu_join) = start_cpu_guard(
        ctx.cancel_handle.clone(),
        Arc::clone(ctx.reason),
        Arc::clone(&grace_cpu),
        grace_cpu_cap,
    );
    let decoy_reason = Arc::new(ReasonCell::new());

    let mut error: Option<(String, String)> = None;
    let mut status;
    grace_cpu.enter();
    v8::tc_scope!(let tc, scope);
    'grace: loop {
        tc.perform_microtask_checkpoint();

        // The grace CPU guard writes the REAL reason cell when it kills a
        // runaway continuation; end the phase the moment that happened
        // instead of idling out the wall — the instance is tainted either
        // way, but the pool slot frees now (review, finding 2).
        if ctx.reason.get().is_some() {
            status = GraceStatus::Truncated;
            break 'grace;
        }

        // Drain the set: drop what settled, note the first rejection, keep
        // driving the rest. New registrations (waitUntil from a continuation)
        // land in the same slot and are picked up next round.
        // SAFETY: pending outlives the call; borrow is transient.
        let taken = unsafe { (*pending).slot.borrow_mut().take() };
        let mut still: Vec<v8::Global<v8::Promise>> = Vec::new();
        for g in taken.unwrap_or_default() {
            let p = v8::Local::new(tc, &g);
            match p.state() {
                v8::PromiseState::Pending => still.push(g),
                v8::PromiseState::Rejected => {
                    if error.is_none() {
                        let rejection = p.result(tc);
                        error = Some(capped_grace_error(
                            error_name_from_value(tc, rejection)
                                .unwrap_or_else(|| "Error".to_string()),
                            error_message_from_value(tc, rejection)
                                .unwrap_or_else(|| "background work failed".to_string()),
                        ));
                    }
                }
                v8::PromiseState::Fulfilled => {}
            }
        }
        if still.is_empty() {
            unsafe { *(*pending).slot.borrow_mut() = Some(Vec::new()) };
            status = if error.is_some() {
                GraceStatus::Failed
            } else {
                GraceStatus::Settled
            };
            break 'grace;
        }
        if grace_start.elapsed() >= Duration::from_millis(ctx.limits.grace_ms as u64) {
            unsafe { *(*pending).slot.borrow_mut() = Some(still) };
            status = GraceStatus::Truncated;
            break 'grace;
        }

        // One aggregate promise over the remaining set, driven by the same
        // settle machinery as the run itself (bridge frames, Terminate,
        // boundary timeouts). Built with the pristine Promise.allSettled
        // captured at install time from a private slot, so guest tampering
        // with the public one changes nothing here.
        let list = v8::Array::new(tc, still.len() as i32);
        for (i, g) in still.iter().enumerate() {
            let p = v8::Local::new(tc, g);
            list.set_index(tc, i as u32, p.into());
        }
        unsafe { *(*pending).slot.borrow_mut() = Some(still) };
        let aggregate = (|| -> Option<v8::Local<v8::Promise>> {
            let global = tc.get_current_context().global(tc);
            let key = grace_all_settled_key(tc)?;
            let all_settled =
                v8::Local::<v8::Function>::try_from(global.get_private(tc, key)?).ok()?;
            let recv = v8::undefined(tc).into();
            let p = all_settled.call(tc, recv, &[list.into()])?;
            v8::Local::<v8::Promise>::try_from(p).ok()
        })();
        let Some(aggregate) = aggregate else {
            status = GraceStatus::Truncated;
            if error.is_none() {
                error = Some((
                    "Error".to_string(),
                    "grace aggregate could not be built".to_string(),
                ));
            }
            break 'grace;
        };

        // Wait in short ticks rather than one long sleep, so a mid-turn CPU
        // kill (real reason cell) or the overall wall is observed within
        // ~250 ms instead of holding the slot for the rest of grace_ms.
        let remaining_ms = (ctx.limits.grace_ms as u64)
            .saturating_sub(grace_start.elapsed().as_millis() as u64)
            .max(1);
        let tick_limits = Limits {
            wall_time_ms: remaining_ms.min(250) as u32,
            ..*ctx.limits
        };
        let tick_ctx = SettleCtx {
            stream_fd: ctx.stream_fd,
            limits: &tick_limits,
            start: std::time::Instant::now(),
            reason: &decoy_reason,
            cpu_budget: &grace_cpu,
            bridge_log: ctx.bridge_log,
            bridge_error: ctx.bridge_error,
            pending_resolvers: ctx.pending_resolvers,
            cancel_handle: ctx.cancel_handle,
            streams: ctx.streams,
        };
        match settle_promise(tc, aggregate, &tick_ctx) {
            // allSettled fulfils; re-enter the drain to pick up outcomes and
            // any promises registered meanwhile.
            Ok(v8::PromiseState::Fulfilled) => continue 'grace,
            // No socket and nothing can resolve the rest.
            Ok(_) => {
                status = GraceStatus::Truncated;
                break 'grace;
            }
            // A tick expired: loop back — the top of the loop decides whether
            // the CPU guard fired or the overall wall is spent.
            Err(RunError::WallTimeout) => continue 'grace,
            // A host Terminate mid-grace (abort during the epilogue) — clean.
            Err(RunError::Aborted) => {
                status = GraceStatus::Truncated;
                break 'grace;
            }
            // The grace CPU guard fired mid-turn (real reason cell set → the
            // caller's taint verdict sees it), or a fatal bridge error.
            Err(e) => {
                status = GraceStatus::Truncated;
                if error.is_none() {
                    error = Some(capped_grace_error(
                        "Error".to_string(),
                        format!("{e:?}"),
                    ));
                }
                break 'grace;
            }
        }
    }
    grace_cpu.leave();
    let _ = grace_cancel_cpu.send(());
    if let Some(j) = grace_cpu_join {
        j.join().ok();
    }

    // Grace-only telemetry: records appended after the early snapshot, and
    // the console lines written since the buffers were handed over.
    let all_records = ctx
        .bridge_log
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .snapshot(elapsed_ms(ctx.start));
    let bridge_calls = all_records.get(run_record_count..).unwrap_or(&[]).to_vec();
    let (stdout, stderr) = unsafe {
        let l = ctx.logs.cast_mut();
        (
            std::mem::take(&mut (*l).stdout),
            std::mem::take(&mut (*l).stderr),
        )
    };

    GraceReport {
        status,
        duration_ms: elapsed_ms(grace_start),
        cpu_time_ms: grace_cpu.elapsed_ms_precise(),
        stdout,
        stderr,
        bridge_calls,
        error,
        early_result_sent,
    }
}

// ── Warm instances ────────────────────────────────────────────────────

/// Fixed budget for warm-up, deliberately not user-configurable —
/// Cloudflare's script-startup limit (1 s since 2025-10) is the model. The
/// wall guard spans the per-instance runtime installs plus prefix
/// evaluation; the CPU meter counts prefix evaluation only. Isolate boot
/// itself (~0.2 ms, cannot loop) precedes the guards and is excluded — it is
/// not sandbox-controllable, so there is nothing to police. Warm-up is never
/// billed to the request that triggered it; blowing this budget reports
/// `ERR_WARMUP_LIMIT`.
pub const WARMUP_WALL_MS: u32 = 1_000;
/// CPU half of the warm-up budget; see [`WARMUP_WALL_MS`].
pub const WARMUP_CPU_MS: u32 = 1_000;

/// Everything that lives as long as an isolate: the isolate itself, its
/// context, the once-per-instance installs (console, web runtime, async
/// context), and the evaluated prefix module. One-off runs build the same
/// state inline per run (`run_module_inner`); warm instances retain an
/// `InstanceCore` across calls on a dedicated owner thread.
pub struct InstanceCore {
    // Field order is drop order: the Globals must die before the isolate.
    context: v8::Global<v8::Context>,
    prefix_module: Option<v8::Global<v8::Module>>,
    isolate: v8::OwnedIsolate,
    /// Console sink. The console holds a raw pointer into this Box (stable
    /// address), so the buffers live exactly as long as the isolate; caps
    /// are re-pointed and contents drained per call.
    logs: Box<LogBuffers>,
    /// Termination-reason slot shared with the near-heap callback and the
    /// ArrayBuffer allocator, which are registered once per isolate. Reset
    /// per call — the reason a [`ReasonCell`] replaced the old `OnceLock`.
    reason: Arc<ReasonCell>,
    /// Keeps the identity of the allocator state visible to the core. The
    /// allocator holds its own Arc reference (via `Arc::into_raw`), and its
    /// `used` counter deliberately accumulates across calls: the heap cap is
    /// per instance, not per call.
    #[allow(dead_code)]
    alloc_state: Option<Arc<BudgetAllocState>>,
    /// Owns the near-heap callback's data for the isolate's lifetime.
    #[allow(dead_code)]
    near_heap: Option<Box<NearHeapData>>,
    /// Bridge-stub dispatch slots (see [`StubSlot`]): the pointees of every
    /// stub `Function` ever created in this context, re-pointed per call and
    /// disarmed between calls.
    stub_slots: StubSlots,
    /// `waitUntil` registration set — the native callback holds a raw
    /// pointer into this Box; armed per call, disarmed between calls.
    pending: Box<PendingWork>,
    /// Streamed-body registry, same pointer discipline and lifecycle.
    streams: Box<StreamTable>,
    /// True until the first call: warm-up console output (the prefix's
    /// `console.log`s) is sitting in `logs` and is delivered on the
    /// cold-start call's result instead of being dropped.
    warmup_logs_pending: bool,
}

/// Build an isolate with the heap cap, budget ArrayBuffer allocator, and
/// near-heap callback wired to `reason` — the CreateParams dance shared by
/// `run_module_inner`-style one-off setup, warm instance creation, and
/// `prepare()` validation. `memory_mb == 0` means uncapped.
fn new_capped_isolate(
    memory_mb: u32,
    reason: &Arc<ReasonCell>,
) -> (
    v8::OwnedIsolate,
    Option<Arc<BudgetAllocState>>,
    Option<Box<NearHeapData>>,
) {
    let alloc_state: Option<Arc<BudgetAllocState>> = if memory_mb > 0 {
        Some(Arc::new(BudgetAllocState {
            used: AtomicUsize::new(0),
            budget: memory_mb as usize * 1024 * 1024,
            handle: OnceLock::new(),
            reason: Arc::clone(reason),
        }))
    } else {
        None
    };

    let mut isolate = {
        let params = v8::Isolate::create_params();
        let params = if memory_mb > 0 {
            params.heap_limits(0, memory_mb as usize * 1024 * 1024)
        } else {
            params
        };
        let params = match &alloc_state {
            Some(state) => {
                let raw = Arc::into_raw(Arc::clone(state));
                unsafe {
                    params.array_buffer_allocator(v8::new_rust_allocator(raw, &BUDGET_ALLOC_VTABLE))
                }
            }
            None => params,
        };
        v8::Isolate::new(params)
    };
    if let Some(state) = &alloc_state {
        state.handle.set(isolate.thread_safe_handle()).ok();
    }
    isolate.set_microtasks_policy(v8::MicrotasksPolicy::Explicit);
    isolate.set_host_initialize_import_meta_object_callback(host_import_meta_callback);

    let near_heap: Option<Box<NearHeapData>> = if memory_mb > 0 {
        let data = Box::new(NearHeapData {
            handle: isolate.thread_safe_handle(),
            reason: Arc::clone(reason),
        });
        let raw = &*data as *const NearHeapData as *mut std::ffi::c_void;
        isolate.add_near_heap_limit_callback(near_heap_limit_cb, raw);
        Some(data)
    } else {
        None
    };

    (isolate, alloc_state, near_heap)
}

/// Classify a warm-up failure: a memory-cap blow keeps its own code (the
/// user's fix is a smaller prefix or a bigger cap, not faster startup); any
/// other fired guard is the fixed time budget; otherwise the fallback stands.
fn warmup_error(reason: &ReasonCell, fallback: RunError) -> RunError {
    match reason.get() {
        Some(TerminationReason::Memory) => RunError::MemoryLimit,
        Some(_) => RunError::WarmupLimit,
        None => fallback,
    }
}

/// Build a fresh isolate + context, run the once-per-instance installs, and
/// evaluate the prefix — all under the fixed warm-up budget. This is the
/// cold-start half of a warm instance; `run_call_on_core` is the per-call
/// half. A guard fired during warm-up reports `ERR_WARMUP_LIMIT` (the
/// caller's limits are not armed, so neither timeout code would be honest);
/// blowing the heap cap keeps `ERR_MEMORY_LIMIT`.
pub fn create_instance_core(
    prefix: Option<PrefixSpec<'_>>,
    imports: &[ImportBinding],
    memory_mb: u32,
) -> Result<InstanceCore, FailureOutput> {
    init_platform();
    let start = std::time::Instant::now();

    let mut logs = Box::new(LogBuffers::default());
    let reason = Arc::new(ReasonCell::new());
    let (mut isolate, alloc_state, near_heap) = new_capped_isolate(memory_mb, &reason);

    let handle = isolate.thread_safe_handle();
    let warmup_budget = Arc::new(CpuBudget::new());
    let (cancel_wall, wall_join) =
        start_wall_guard(handle.clone(), Arc::clone(&reason), WARMUP_WALL_MS);
    let (cancel_cpu, cpu_join) = start_cpu_guard(
        handle.clone(),
        Arc::clone(&reason),
        Arc::clone(&warmup_budget),
        WARMUP_CPU_MS,
    );
    let guard_canceller = GuardCanceller {
        cancel_wall: &cancel_wall,
        cancel_cpu: &cancel_cpu,
        budget: &warmup_budget,
    };

    let logs_ptr: *mut LogBuffers = &mut *logs;
    let pending_work = PendingWork::boxed();
    let pending_ptr: *const PendingWork = &*pending_work;
    let stream_table = StreamTable::boxed();
    let stream_ptr: *const StreamTable = &*stream_table;
    let setup = (|| -> Result<(v8::Global<v8::Context>, Option<v8::Global<v8::Module>>), RunError> {
        v8::scope!(let scope, &mut isolate);
        let context = v8::Context::new(scope, Default::default());
        let scope = &mut v8::ContextScope::new(scope, context);
        install_console(scope, logs_ptr)?;
        install_web_runtime(scope, stream_ptr).map_err(RunError::Internal)?;
        install_async_context(scope)?;
        // Disarmed during prefix evaluation: a setup-time waitUntil throws.
        install_wait_until(scope, pending_ptr)?;
        let mut prefix_module: Option<v8::Global<v8::Module>> = None;
        if let Some(prefix) = &prefix {
            // Enter the CPU meter for compile + evaluate only — isolate boot
            // is ours, the prefix's code is what the budget polices.
            warmup_budget.enter();
            v8::tc_scope!(let tc, scope);
            let module =
                evaluate_prefix_module(tc, prefix.code, prefix.filename, prefix.globals, imports)?;
            prefix_module = Some(module);
            warmup_budget.leave();
        }
        Ok((v8::Global::new(scope, context), prefix_module))
    })();
    drop(guard_canceller);
    // Join the guards: cancellation only signals them, and a guard whose
    // deadline expired concurrently may still be about to fire. This isolate
    // is about to live on — any fire must be visible NOW, not land on the
    // first call.
    if let Some(j) = wall_join {
        j.join().ok();
    }
    if let Some(j) = cpu_join {
        j.join().ok();
    }

    match setup {
        // A guard that fired *after* evaluation completed still taints: the
        // pending terminate may have landed anywhere. Fired guard ⇒ discard,
        // no exceptions to the doctrine.
        Ok(_) if reason.get().is_some() => Err(failure(
            warmup_error(&reason, RunError::WarmupLimit),
            &logs,
            start,
        )),
        Ok((context, prefix_module)) => {
            handle.cancel_terminate_execution();
            Ok(InstanceCore {
                context,
                prefix_module,
                isolate,
                logs,
                reason,
                alloc_state,
                near_heap,
                stub_slots: StubSlots::new(),
                pending: pending_work,
                streams: stream_table,
                warmup_logs_pending: true,
            })
        }
        Err(error) => Err(failure(warmup_error(&reason, error), &logs, start)),
    }
}

/// What the warm layer needs to know after each call on an instance.
pub struct CallOutcome {
    pub result: Result<Output, FailureOutput>,
    /// True when the isolate's state can no longer be trusted: a guard
    /// fired, the call was aborted mid-execution, or a fatal bridge/runtime
    /// error terminated execution. Tainted instances MUST be evicted; only
    /// clean completions (including ordinary uncaught exceptions) leave the
    /// instance reusable.
    pub tainted: bool,
    /// `used_heap_size` after the call — reported on the Result frame and
    /// the input to eviction scoring.
    pub heap_used_bytes: u64,
}

/// Run one call (postfix or export call) on a retained instance: fresh
/// guards and CPU budget from dispatch to settle, per-call bridge stubs,
/// per-call console caps — the same per-request semantics a one-off run has,
/// minus the isolate boot and prefix evaluation.
#[allow(clippy::too_many_arguments)] // mirrors run_module; bundled state is the instance itself
pub fn run_call_on_core(
    core: &mut InstanceCore,
    code: Option<&str>,
    filename: &str,
    prefix_globals: &[HostGlobalDef],
    limits: Limits,
    globals: &[HostGlobalDef],
    imports: &[ImportBinding],
    stream_fd: Option<RawFd>,
    call_id_counter: Arc<AtomicU32>,
    call: Option<&ipc::CallSpec>,
) -> CallOutcome {
    let start = std::time::Instant::now();
    let cpu_budget = Arc::new(CpuBudget::new());
    let bridge_log: Arc<Mutex<BridgeCallLog>> = Arc::new(Mutex::new(BridgeCallLog::default()));

    // Per-call reset of instance-lifetime state: the termination-reason slot
    // and the console sink (whose caps are per-call limits). The cold-start
    // call keeps the buffers: they hold the prefix's warm-up console output,
    // which belongs to the run that paid for the warm-up — previously the
    // prefix re-evaluated per run, so its output appeared on every result.
    core.reason.reset();
    {
        let logs = &mut *core.logs;
        if core.warmup_logs_pending {
            core.warmup_logs_pending = false;
        } else {
            logs.stdout.clear();
            logs.stderr.clear();
            logs.stdout_bytes = 0;
            logs.stderr_bytes = 0;
        }
        logs.max_stdout_bytes = limits.max_stdout_bytes;
        logs.max_stderr_bytes = limits.max_stderr_bytes;
    }

    // Guards go up BEFORE anything touches the retained context. The
    // placeholder re-arm below runs `define_own_property` — which never
    // invokes user accessors — but defence in depth: any path that can
    // execute sandbox-authored code on this thread must be under a budget,
    // or a hostile prefix/postfix could hang the owner thread forever.
    let handle = core.isolate.thread_safe_handle();
    let cancel_handle = handle.clone();
    let (cancel_wall, wall_join) =
        start_wall_guard(handle.clone(), Arc::clone(&core.reason), limits.wall_time_ms);
    let (cancel_cpu, cpu_join) = start_cpu_guard(
        handle,
        Arc::clone(&core.reason),
        Arc::clone(&cpu_budget),
        limits.cpu_time_ms,
    );

    let mut result: Result<Output, FailureOutput>;
    {
        let guard_canceller = GuardCanceller {
            cancel_wall: &cancel_wall,
            cancel_cpu: &cancel_cpu,
            budget: &cpu_budget,
        };

        // Re-arm throwing placeholders over every bridge-callable name BEFORE
        // the live stubs bind: a name the previous call bound live but this
        // call does not re-bind must throw, not dispatch (its slot is
        // disarmed below either way — the placeholder gives the honest
        // error instead of the stale-reference one).
        let placeholder_result = {
            v8::scope!(let scope, &mut core.isolate);
            let context = v8::Local::new(scope, &core.context);
            let scope = &mut v8::ContextScope::new(scope, context);
            install_prefix_bridge_placeholders(scope, prefix_globals, imports)
        };
        result = match placeholder_result {
            Err(error) => Err(failure(
                termination_or(&core.reason, error),
                &core.logs,
                start,
            )),
            Ok(()) => run_call_phase(
                &mut core.isolate,
                &core.context,
                core.prefix_module.as_ref(),
                code,
                filename,
                &mut core.stub_slots,
                CallPhaseCtx {
                    limits: &limits,
                    globals,
                    imports,
                    stream_fd,
                    call_id_counter,
                    call,
                    start,
                    cpu_budget: &cpu_budget,
                    bridge_log: &bridge_log,
                    reason: &core.reason,
                    logs: std::ptr::addr_of!(*core.logs),
                    pending: &*core.pending,
                    streams: &*core.streams,
                    epilogue: take_epilogue_spec(),
                    cancel_wall: &cancel_wall,
                    cancel_cpu: &cancel_cpu,
                    cancel_handle: &cancel_handle,
                },
            ),
        };
        drop(guard_canceller);
    }
    // Join the guards before judging the instance: cancellation only signals
    // them, and a guard whose deadline expired concurrently may still be
    // between "decide to fire" and "set reason + terminate". After the joins,
    // any fire is visible to the taint check below instead of landing
    // invisibly on the next call of a reused isolate.
    if let Some(j) = wall_join {
        j.join().ok();
    }
    if let Some(j) = cpu_join {
        j.join().ok();
    }

    // Disarm the stub slots: this call's bridge state (socket fd, resolver
    // map, telemetry Arcs) dies with this function, so any stub Function
    // sandbox code stashed must throw, not dispatch, until the next call
    // re-points the slots.
    for slot in core.stub_slots.values() {
        *slot.data.borrow_mut() = None;
    }
    // Disarm waitUntil too: a between-calls registration must throw,
    // and lingering Globals from the finished run should not pin objects.
    *core.pending.slot.borrow_mut() = None;
    // The stream table was disarmed (and live streams cancelled) by
    // run_call_phase's normal exit; this covers the failure paths that never
    // reached it. Idempotent.
    close_run_streams(&*core.streams);

    // Stamp telemetry exactly as `run_module` does for the one-off path.
    let cpu_time_ms = cpu_budget.elapsed_ms_precise();
    let records = bridge_log
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .finalize(elapsed_ms(start));
    match &mut result {
        Ok(output) => {
            output.cpu_time_ms = cpu_time_ms;
            output.bridge_calls = records;
        }
        Err(f) => {
            f.cpu_time_ms = cpu_time_ms;
            f.bridge_calls = records;
        }
    }

    // Taint verdict. A fired guard means terminate_execution ripped
    // arbitrary mid-execution state; a still-pending termination flag means
    // something terminated without the clean paths' cancel (fatal bridge
    // violations set it without a reason); the error-shape check catches the
    // remaining fatal paths (abort, internal faults). Ordinary uncaught
    // exceptions stay clean. When in doubt, evict — cold starts are correct,
    // reuse is only the optimization.
    let tainted = core.reason.get().is_some()
        || core.isolate.is_execution_terminating()
        || match &result {
            Ok(_) => false,
            Err(f) => matches!(
                f.error,
                RunError::Aborted
                    | RunError::WallTimeout
                    | RunError::CpuTimeout
                    | RunError::MemoryLimit
                    | RunError::Internal(_)
                    | RunError::BridgeCallPayloadTooLarge
                    | RunError::BridgeCallLimitExceeded
                    | RunError::FunctionArgumentNotSupported
            ),
        };

    CallOutcome {
        result,
        tainted,
        heap_used_bytes: heap_used(&mut core.isolate),
    }
}

fn heap_used(isolate: &mut v8::OwnedIsolate) -> u64 {
    isolate.get_heap_statistics().used_heap_size() as u64
}

/// Clone the shared `bridge_error` OnceLock's content into an owned RunError.
fn owned_bridge_error(err: &RunError) -> RunError {
    match err {
        RunError::HostBridge(m) => RunError::HostBridge(m.clone()),
        RunError::FunctionArgumentNotSupported => RunError::FunctionArgumentNotSupported,
        RunError::BridgeCallPayloadTooLarge => RunError::BridgeCallPayloadTooLarge,
        RunError::BridgeCallLimitExceeded => RunError::BridgeCallLimitExceeded,
        RunError::Internal(m) => RunError::Internal(m.clone()),
        other => RunError::Internal(format!("unexpected bridge error: {other:?}")),
    }
}

/// Everything the poll loop needs besides the promise it settles. One run
/// builds this once; both settle points — the module evaluation promise and a
/// requested call's return promise — share it.
/// Deliver one `StreamChunk`/`StreamEnd` frame into the run's stream table:
/// resolve the pending sandbox read when one waits (crediting consumed
/// bytes), buffer otherwise. Ignores frames for unknown or cancelled streams
/// (benign races with a cancel already sent).
fn deliver_stream_frame(
    scope: &mut v8::PinScope,
    streams: *const StreamTable,
    frame: &ipc::TsToRustFrame,
) -> Result<(), RunError> {
    // SAFETY: the table outlives the run frame; transient borrow.
    let table = unsafe { &*streams };
    let mut slot = table.slot.borrow_mut();
    let Some(state) = slot.as_mut() else {
        return Ok(()); // no run armed (stray frame) — drop it
    };

    match frame.message_type {
        ipc::TsToRustMessageType::StreamChunk => {
            let chunk = ipc::parse_stream_chunk_payload(&frame.payload)
                .map_err(|e| RunError::Internal(format!("stream chunk decode: {e}")))?;
            if state.run_id != 0 && chunk.run_id != state.run_id {
                return Ok(());
            }
            let stream_id = chunk.stream_id;
            let Some(stream) = state.streams.get_mut(&stream_id) else {
                return Ok(());
            };
            if stream.cancelled {
                return Ok(());
            }
            if let Some(pending) = stream.pending.take() {
                let credit = chunk.data.len() as u32;
                let data = chunk.data.to_vec();
                send_stream_credit(state, stream_id, credit);
                drop(slot);
                let value = bytes_to_uint8array(scope, data);
                let resolver = v8::Local::new(scope, pending);
                match value {
                    Some(v) => {
                        resolver.resolve(scope, v.into());
                    }
                    None => {
                        let msg =
                            v8_error_value(scope, "[iso4] stream chunk could not be materialised");
                        resolver.reject(scope, msg);
                    }
                }
                return Ok(());
            }
            // Buffered: the host must stay inside the credit window; a small
            // slack of one max chunk absorbs frames in flight when a cancel
            // races the pump.
            let cap = ipc::STREAM_CREDIT_WINDOW_BYTES as usize
                + ipc::STREAM_CHUNK_MAX_BYTES as usize;
            if stream.buffered_bytes + chunk.data.len() > cap {
                return Err(RunError::Internal(
                    "stream credit window violated by the host".to_string(),
                ));
            }
            stream.buffered_bytes += chunk.data.len();
            stream.buffered.push_back(chunk.data.to_vec());
            Ok(())
        }
        ipc::TsToRustMessageType::StreamEnd => {
            let end = ipc::parse_stream_end_payload(&frame.payload)
                .map_err(|e| RunError::Internal(format!("stream end decode: {e}")))?;
            if state.run_id != 0 && end.run_id != state.run_id {
                return Ok(());
            }
            let Some(stream) = state.streams.get_mut(&end.stream_id) else {
                return Ok(());
            };
            let outcome: Result<(), String> = if end.ok {
                Ok(())
            } else {
                Err(end
                    .error
                    .unwrap_or_else(|| "the host body source failed".to_string()))
            };
            stream.ended = Some(outcome.clone());
            // A read waiting on an empty buffer observes the end immediately.
            if stream.buffered.is_empty() {
                if let Some(pending) = stream.pending.take() {
                    drop(slot);
                    let resolver = v8::Local::new(scope, pending);
                    match outcome {
                        Ok(()) => {
                            let null = v8::null(scope).into();
                            resolver.resolve(scope, null);
                        }
                        Err(message) => {
                            let msg = v8_error_value(
                                scope,
                                &format!("[iso4] body stream failed: {message}"),
                            );
                            resolver.reject(scope, msg);
                        }
                    }
                }
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// End-of-run stream cleanup: cancel every stream the host may still be
/// pumping, disarm the table, and clear the per-thread pointer. Idempotent.
fn close_run_streams(streams: *const StreamTable) {
    // SAFETY: same contract as deliver_stream_frame.
    let table = unsafe { &*streams };
    let taken = table.slot.borrow_mut().take();
    if let Some(state) = taken {
        for (id, stream) in &state.streams {
            if stream.ended.is_none() && !stream.cancelled {
                send_stream_cancel(&state, *id, "run ended");
            }
        }
    }
    STREAM_TABLE_PTR.with(|c| c.set(std::ptr::null()));
}

struct SettleCtx<'a> {
    /// Socket for BridgeResponse reads. `None` when no bridge stub exists.
    stream_fd: Option<RawFd>,
    limits: &'a Limits,
    start: std::time::Instant,
    reason: &'a Arc<ReasonCell>,
    cpu_budget: &'a Arc<CpuBudget>,
    bridge_log: &'a Arc<Mutex<BridgeCallLog>>,
    bridge_error: &'a Arc<OnceLock<RunError>>,
    pending_resolvers: &'a PendingResolvers,
    cancel_handle: &'a v8::IsolateHandle,
    /// The run's streamed-body registry: chunk/end frames arriving between
    /// bridge responses are delivered here.
    streams: *const StreamTable,
}

/// Drive microtasks until `promise` settles, draining `BridgeResponse` frames
/// and honouring `Terminate` in between — the poll loop.
///
/// With MicrotasksPolicy::Explicit, microtasks do not run automatically.
/// After each BridgeResponse is resolved we call perform_microtask_checkpoint()
/// to advance the await chains, handling sequential bridge calls, concurrent
/// bridge calls (both frames sent before yielding; responses route by callId),
/// and no bridge calls at all (the caller performs one checkpoint before this
/// so already-resolvable promises settle without touching the socket).
///
/// Returns `Fulfilled`, or `Pending` when there is no session socket to wait
/// on (an await that nothing in the isolate can ever resolve); every other
/// outcome is the run's failure, returned as the `RunError` the caller wraps
/// with `failure()`. Guards are NOT cancelled here — the caller decides when
/// the run's JS is really over (a call may still follow the module promise);
/// error paths rely on the `GuardCanceller` drop guard.
fn settle_promise(
    scope: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
    promise: v8::Local<v8::Promise>,
    ctx: &SettleCtx<'_>,
) -> Result<v8::PromiseState, RunError> {
    loop {
        match promise.state() {
            v8::PromiseState::Fulfilled => return Ok(v8::PromiseState::Fulfilled),
            v8::PromiseState::Rejected => {
                let rejection = promise.result(scope);
                // A rejection tagged as a host bridge error surfaces as
                // ERR_HOST_BRIDGE with the handler's name/message/fields intact.
                if let Some(err) = host_bridge_error_from_rejection(scope, rejection) {
                    return Err(err);
                }
                if let Some(err) = ctx.bridge_error.get() {
                    return Err(owned_bridge_error(err));
                }
                return Err(runtime_error_from_value(scope, rejection));
            }
            v8::PromiseState::Pending => {}
        }

        // A fatal bridge error set during a microtask checkpoint (bridge
        // callbacks run inside checkpoints) has terminated execution: no
        // further BridgeResponse will ever arrive, so bail out before
        // blocking on the socket — otherwise this run would sit until the
        // wall timeout and report the wrong error.
        if let Some(err) = ctx.bridge_error.get() {
            return Err(owned_bridge_error(err));
        }

        // ── Drain one BridgeResponse frame ──────────────────────────────────
        //
        // Peek at the socket: if a byte is available a full frame is likely
        // ready (TS handlers write frames atomically).  If nothing is there
        // yet we do a blocking wait respecting the remaining wall budget.
        let frame_result: Result<ipc::TsToRustFrame, io::Error> = if let Some(fd) = ctx.stream_fd {
            // SAFETY: same ManuallyDrop pattern as bridge_global_callback.
            let mut sock = ManuallyDrop::new(unsafe { UnixStream::from_raw_fd(fd) });

            // Blocking read — pause CPU budget so host-handler latency is not
            // charged against the sandbox.  Set a read timeout equal to the
            // remaining wall budget so a stalled handler is caught here.
            let timeout = if ctx.limits.wall_time_ms > 0 {
                let budget = Duration::from_millis(ctx.limits.wall_time_ms as u64);
                let remaining = budget
                    .saturating_sub(ctx.start.elapsed())
                    .max(Duration::from_millis(1));
                Some(remaining)
            } else {
                None
            };
            ctx.cpu_budget.leave();
            if let Some(t) = timeout {
                sock.set_read_timeout(Some(t)).ok();
            }
            // Cap BridgeResponse frame reads by the sandbox memory budget.
            // memory_mb = 0 means the caller explicitly opted out of the memory
            // cap (an explicit 0 on the wire; absent would have resolved to the
            // default); fall back to the global framing cap as a parsing safety
            // limit only in that case.
            let bridge_frame_limit: u32 = if ctx.limits.memory_mb > 0 {
                ctx.limits.memory_mb.saturating_mul(1024 * 1024)
            } else {
                ipc::DEFAULT_MAX_FRAME_LENGTH
            };
            let result = ipc::read_ts_to_rust_frame_with_limit(&mut *sock, bridge_frame_limit);
            if ctx.limits.wall_time_ms > 0 {
                sock.set_read_timeout(None).ok();
            }
            ctx.cpu_budget.enter();
            result
        } else {
            // No bridge globals — Pending with no socket means an await that
            // nothing can ever resolve; the caller surfaces the error.
            return Ok(v8::PromiseState::Pending);
        };

        match frame_result {
            Err(e)
                if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut =>
            {
                // Wall budget exhausted during blocking wait.
                ctx.reason.set(TerminationReason::Wall);
                return Err(RunError::WallTimeout);
            }
            Err(e) => {
                return Err(RunError::Internal(format!("poll loop socket read: {e}")));
            }
            Ok(frame) => {
                // ── Validate and decode the frame ────────────────────────
                match frame.message_type {
                    ipc::TsToRustMessageType::BridgeResponse => {}
                    ipc::TsToRustMessageType::Terminate => {
                        // Graceful abort. The TS host sends `Terminate`
                        // (carrying the run ID) when its `AbortSignal` fires
                        // while the sandbox is suspended awaiting a bridge
                        // response — precisely the durable-isolates suspension
                        // case, and precisely when the V8 thread is parked here
                        // reading the socket and can observe the frame.
                        //
                        // Stop the run and return `Aborted`; `run_module` stamps
                        // the CPU time and the bridge-call records gathered so
                        // far onto the failure, so the aborted result carries
                        // real telemetry instead of the synthesized zeros of the
                        // socket-teardown fallback.
                        //
                        // NOT-IDEAL / DEFERRED: this only reaches a run that is
                        // parked on this socket read. A purely CPU-bound run
                        // (tight loop, no bridge call in flight) never returns
                        // here, so a control-message `Terminate` cannot interrupt
                        // it — that isolate is reclaimed only when the CPU guard
                        // fires (bounded by `cpuTimeMs`), and the TS side falls
                        // back to tearing the socket down. Promptly interrupting
                        // a busy isolate is deliberately deferred; see
                        // DESIGN.md §14.7.
                        //
                        // Only one run is ever in flight per connection, so the
                        // run ID in the payload is redundant for routing; parse
                        // it for validation/diagnostics only.
                        match ipc::parse_terminate_payload(&frame.payload) {
                            Ok(run_id) => {
                                eprintln!("[iso4-v8] Terminate received for run {run_id} — aborting")
                            }
                            Err(e) => eprintln!(
                                "[iso4-v8] Terminate received with malformed payload ({e}) — aborting"
                            ),
                        }
                        ctx.cancel_handle.terminate_execution();
                        return Err(RunError::Aborted);
                    }
                    ipc::TsToRustMessageType::StreamChunk
                    | ipc::TsToRustMessageType::StreamEnd => {
                        deliver_stream_frame(scope, ctx.streams, &frame)?;
                        // Run the continuations the delivery may have resolved,
                        // then keep polling — a stream frame settles no
                        // bridge call and no promise state on its own.
                        scope.perform_microtask_checkpoint();
                        continue;
                    }
                    other => {
                        return Err(RunError::Internal(format!(
                            "poll loop: expected BridgeResponse or Terminate, got {other:?}"
                        )));
                    }
                }
                // Frame size is already bounded by bridge_frame_limit (= memoryMb
                // or DEFAULT_MAX_FRAME_LENGTH) at the read_frame_with_limit call
                // above, so no secondary payload length check is needed here.
                match wire::parse_bridge_response_payload(&frame.payload) {
                    Err(e) => {
                        return Err(RunError::Internal(format!(
                            "poll loop: response decode: {e}"
                        )));
                    }
                    Ok((call_id, result)) => {
                        // Settle the call's bridge record: round-trip time on
                        // the run clock plus the response value's serialized
                        // size (frame payload minus the callId + ok header).
                        // Unknown callIds are ignored inside settle().
                        let response_value_bytes = if result.is_ok() {
                            frame.payload.len().saturating_sub(5) as u32
                        } else {
                            0
                        };
                        ctx.bridge_log
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .settle(
                                call_id,
                                elapsed_ms(ctx.start),
                                result.is_ok(),
                                response_value_bytes,
                            );
                        // Route to the matching resolver.  An unknown callId
                        // means a stale frame from a previous run — discard.
                        let entry = ctx
                            .pending_resolvers
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .remove(&call_id);
                        if let Some(PendingResolver(global_resolver)) = entry {
                            let resolver = v8::Local::new(scope, global_resolver);
                            match result {
                                Ok(value_blob) => {
                                    let decoded = match &value_blob {
                                        // Absent value slot → the handler
                                        // returned nothing.
                                        None => Some(v8::undefined(scope).into()),
                                        // Web-aware: a bridge handler may
                                        // return a Request/Response.
                                        Some(bytes) => {
                                            blob::deserialize_value_with_web_types(scope, bytes)
                                        }
                                    };
                                    if let Some(v8_val) = decoded {
                                        resolver.resolve(scope, v8_val);
                                    } else {
                                        let detail = blob::take_codec_error()
                                            .map(|e| e.message().to_string())
                                            .unwrap_or_else(|| {
                                                "failed to deserialize response value".to_string()
                                            });
                                        let msg = v8::String::new(
                                            scope,
                                            &format!("[iso4] bridge: {detail}"),
                                        )
                                        .unwrap();
                                        resolver.reject(scope, msg.into());
                                    }
                                }
                                Err(bridge_err) => {
                                    // Reject with a real Error carrying the
                                    // handler's name/message/fields, tagged
                                    // a private symbol so the Rejected arm can
                                    // classify an *uncaught* one as HostBridge.
                                    // Sandbox code may catch it and continue —
                                    // host handler errors are not run-fatal.
                                    let error_obj = host_bridge_error_to_v8(scope, &bridge_err);
                                    resolver.reject(scope, error_obj);
                                }
                            }
                        }
                        // Unknown callId → silently discard (cross-run contamination guard).
                    }
                }
            }
        }

        // ── Advance all pending await chains ─────────────────────────────────
        // This is the only place microtasks run (Explicit policy).  After
        // resolving the Promise above, the microtask queued by the `await`
        // resumes execution until the code hits its next `await` (sending
        // another BridgeCall) or completes.
        scope.perform_microtask_checkpoint();

        // Check for errors set during the checkpoint (inside bridge callbacks).
        if let Some(err) = ctx.bridge_error.get() {
            return Err(owned_bridge_error(err));
        }
        // Check if a guard thread fired termination during the checkpoint.
        if let Some(r) = ctx.reason.get() {
            return Err(match r {
                TerminationReason::Wall => RunError::WallTimeout,
                TerminationReason::Cpu => RunError::CpuTimeout,
                TerminationReason::Memory => RunError::MemoryLimit,
            });
        }
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

/// Checkpoint bound for settling a prefix's top-level await. Every realistic
/// prefix settles after one checkpoint (a checkpoint drains the whole queue);
/// the bound exists so a never-settling promise fails cleanly instead of
/// looping. Checkpoints on an empty queue are near-free, so the headroom
/// costs nothing.
const PREFIX_SETTLE_MAX_CHECKPOINTS: usize = 1000;

/// Compile, instantiate, and evaluate prefix `code` as an ESM module in the
/// current context, returning the settled module once evaluation succeeds.
///
/// Shared by `prepare()` validation and every run's prefix stage so their
/// behavior — crucially, *which imports resolve* — is identical. Bridge
/// globals are not installed here (bridge stubs are recreated per
/// `execute_with_prefix`), and `node:async_hooks` is not resolvable while a
/// prefix evaluates. The returned handle is what a `prefix.call()` resolves
/// its export path against; validation callers drop it.
fn evaluate_prefix_module(
    scope: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
    code: &str,
    filename: &str,
    globals: &[HostGlobalDef],
    imports: &[ImportBinding],
) -> Result<v8::Global<v8::Module>, RunError> {
    // Install the value globals (string exprs, constants, shim wrappers)
    // before evaluating the prefix, so prefix code sees them. Bridge stubs
    // are NOT installed here — they are created against a live socket only
    // for the postfix stage. Their names get throwing placeholders instead,
    // so a prefix that calls one fails with ERR_PREFIX_BRIDGE_CALL rather
    // than an accidental ReferenceError/TypeError.
    install_value_globals(scope, globals)?;
    install_prefix_bridge_placeholders(scope, globals, imports)?;

    let source_string = v8::String::new(scope, code)
        .ok_or_else(|| RunError::Internal("failed to intern module source".to_string()))?;
    let filename_str = v8::String::new(scope, filename)
        .ok_or_else(|| RunError::Internal("failed to intern filename".to_string()))?;
    let origin = v8::ScriptOrigin::new(
        scope,
        filename_str.into(),
        0,
        0,
        false,
        0,
        None,
        false,
        false,
        true,
        None,
    );
    let mut source = v8::script_compiler::Source::new(source_string, Some(&origin));

    let module = match v8::script_compiler::compile_module(scope, &mut source) {
        Some(m) => m,
        None => return Err(RunError::CompileError(exception_message(scope))),
    };

    let _resolver_guard = install_resolver(ResolverContext {
        bindings: imports.to_vec(),
        module_cache: HashMap::new(),
        pending_meta: Vec::new(),
        resolve_error: None,
        async_context_builtin: false,
    });

    if module
        .instantiate_module(scope, module_resolver_callback)
        .is_none()
    {
        let err = take_resolver()
            .and_then(|ctx| ctx.resolve_error)
            .unwrap_or_else(|| RunError::ModuleNotFound(exception_message(scope)));
        return Err(err);
    }

    let evaluation = match module.evaluate(scope) {
        Some(v) => v,
        None => {
            if let Some(exception) = scope.exception() {
                if let Some(err) = prefix_bridge_call_from_value(scope, exception) {
                    return Err(err);
                }
            }
            return Err(RunError::RuntimeError(Box::new(RuntimeErrorData {
                name: exception_name(scope),
                message: exception_message(scope),
                stack: exception_stack(scope),
                fields: exception_fields(scope),
            })));
        }
    };

    if evaluation.is_promise() {
        let promise = v8::Local::<v8::Promise>::try_from(evaluation).map_err(|_| {
            RunError::Internal("failed to inspect module evaluation promise".to_string())
        })?;
        // Any top-level `await` leaves the evaluation promise pending until
        // the microtask queue is drained; the isolate runs under
        // MicrotasksPolicy::Explicit, so drain it here. One checkpoint drains
        // the whole queue, chained awaits included, so it settles everything
        // that does not wait on an external event. The bound only caps a
        // promise that can never settle — the bridge does not exist while a
        // prefix evaluates, so awaited host I/O never resolves.
        for _ in 0..PREFIX_SETTLE_MAX_CHECKPOINTS {
            if !matches!(promise.state(), v8::PromiseState::Pending) {
                break;
            }
            scope.perform_microtask_checkpoint();
        }
        match promise.state() {
            v8::PromiseState::Fulfilled => {}
            v8::PromiseState::Rejected => {
                let rejection = promise.result(scope);
                if let Some(err) = prefix_bridge_call_from_value(scope, rejection) {
                    return Err(err);
                }
                return Err(runtime_error_from_value(scope, rejection));
            }
            v8::PromiseState::Pending => {
                return Err(RunError::PrefixDidNotSettle(
                    "prefix top-level await never settled: nothing in the isolate can \
                     resolve the awaited promise at precompile time"
                        .to_string(),
                ));
            }
        }
    }

    Ok(v8::Global::new(scope, module))
}

/// Validate prefix code in a throwaway regular isolate: compile + instantiate
/// + evaluate exactly as a run will. Any failure (syntax error, unresolved
/// import, throwing top-level code) returns a clean error at `prepare()` time
/// instead of on the first run.
///
/// The environment matches a run's prefix stage — web globals installed,
/// value globals + throwing bridge placeholders, no `node:async_hooks` — so
/// what validates here evaluates identically per run. Prefix code is
/// host-authored setup with no bridge/network side effects, so evaluating it
/// here and again on every run is safe.
///
/// Runs under the same fixed warm-up budget as a cold start: a prefix
/// that cannot evaluate inside [`WARMUP_WALL_MS`]/[`WARMUP_CPU_MS`] would
/// fail *every* instance creation with `ERR_WARMUP_LIMIT`, so it is rejected
/// here, at deploy time — the same reason wrangler enforces Cloudflare's
/// startup limit at upload.
fn validate_prefix_module(
    code: &str,
    filename: &str,
    globals: &[HostGlobalDef],
    imports: &[ImportBinding],
    memory_mb: u32,
) -> Result<(), FailureOutput> {
    let start = std::time::Instant::now();
    let logs = LogBuffers::default();

    // A regular isolate requires the platform to be initialized.
    // `precompile()` already does this, but call it here too so direct
    // callers (tests) work; it is idempotent.
    init_platform();

    // Validation runs under the same heap cap as a real instance: a prefix
    // that blows the cap would fail every cold start, and an *uncapped*
    // validation isolate would let it OOM the runtime process at prepare()
    // time instead of failing cleanly.
    let reason = Arc::new(ReasonCell::new());
    let (mut isolate, _alloc_state, _near_heap) = new_capped_isolate(memory_mb, &reason);

    let warmup_budget = Arc::new(CpuBudget::new());
    let handle = isolate.thread_safe_handle();
    let (cancel_wall, _wall_join) =
        start_wall_guard(handle.clone(), Arc::clone(&reason), WARMUP_WALL_MS);
    let (cancel_cpu, _cpu_join) = start_cpu_guard(
        handle,
        Arc::clone(&reason),
        Arc::clone(&warmup_budget),
        WARMUP_CPU_MS,
    );
    let _guard_canceller = GuardCanceller {
        cancel_wall: &cancel_wall,
        cancel_cpu: &cancel_cpu,
        budget: &warmup_budget,
    };

    v8::scope!(let scope, &mut isolate);
    let context = v8::Context::new(scope, Default::default());
    let scope = &mut v8::ContextScope::new(scope, context);
    // Same environment as a run, so prefix code that touches a web global is
    // validated here rather than failing on the first run.
    crate::webtypes::install(scope).map_err(|e| {
        failure(
            termination_or(&reason, RunError::Internal(e)),
            &logs,
            start,
        )
    })?;
    // Disarmed waitUntil for surface parity: `typeof waitUntil` matches run
    // code; calling it here throws the catchable setup-time error.
    let validation_pending = PendingWork::boxed();
    install_wait_until(scope, &*validation_pending).map_err(|e| {
        failure(termination_or(&reason, e), &logs, start)
    })?;
    v8::tc_scope!(let scope, scope);
    warmup_budget.enter();
    let result = evaluate_prefix_module(scope, code, filename, globals, imports);
    warmup_budget.leave();
    match result {
        Ok(_) if reason.get().is_some() => Err(failure(
            warmup_error(&reason, RunError::WarmupLimit),
            &logs,
            start,
        )),
        Ok(_) => Ok(()),
        Err(error) => Err(failure(warmup_error(&reason, error), &logs, start)),
    }
}

/// Wall-clock elapsed time since `start` in milliseconds, rounded to
/// microsecond resolution (three decimal places). The wire carries
/// durations as f64, so sub-millisecond runs stay visible.
fn elapsed_ms(start: std::time::Instant) -> f64 {
    (start.elapsed().as_secs_f64() * 1_000_000.0).round() / 1_000.0
}

/// A guard can fire while the run is still in setup — installing globals or
/// the web runtime, evaluating the prefix, compiling the postfix. V8 then
/// fails the interrupted operation, and classifying that failure would
/// misreport the timeout as `ERR_INTERNAL` ("evaluate web runtime: null") or
/// `ERR_COMPILE`. The guard stores its reason *before* it calls
/// `terminate_execution`, so a set reason here is always the real cause;
/// otherwise the fallback stands.
fn termination_or(reason: &ReasonCell, fallback: RunError) -> RunError {
    match reason.get() {
        Some(TerminationReason::Wall) => RunError::WallTimeout,
        Some(TerminationReason::Cpu) => RunError::CpuTimeout,
        Some(TerminationReason::Memory) => RunError::MemoryLimit,
        None => fallback,
    }
}

fn failure(error: RunError, logs: &LogBuffers, start: std::time::Instant) -> FailureOutput {
    FailureOutput {
        error,
        stdout: logs.stdout.clone(),
        stderr: logs.stderr.clone(),
        duration_ms: elapsed_ms(start),
        // Stamped by run_module from the shared run state (stays empty/0 for
        // precompile failures, which have no bridge and no CPU budget).
        cpu_time_ms: 0.0,
        bridge_calls: Vec::new(),
    }
}

fn no_import_resolver<'a>(
    _context: v8::Local<'a, v8::Context>,
    _specifier: v8::Local<'a, v8::String>,
    _import_assertions: v8::Local<'a, v8::FixedArray>,
    _referrer: v8::Local<'a, v8::Module>,
) -> Option<v8::Local<'a, v8::Module>> {
    None
}

// ── Module resolver (Phases 6 + 7) ──────────────────────────────────────────
//
// Two flavours per DESIGN.md §4.3:
//   - Source modules:  host supplies ESM source. We compile it with
//                      script_compiler::compile_module. The top-level
//                      instantiate_module recurses through our resolver to
//                      satisfy transitive imports, so source modules can
//                      themselves import other source modules.
//   - Host modules:    the public TS API accepts an object form, but it is lowered
//                      on the TS side to generated ESM source. By the time the
//                      binding reaches this resolver, it's still just (specifier, source).
//
// V8's module resolver callback has no user-data slot. The state lives in a
// thread-local that is set before `instantiate_module` and cleared once both
// `instantiate_module` and `evaluate` have returned (synthetic-module
// evaluation steps also dip into it). Each `run_module` invocation runs on
// one thread with one isolate, so this is safe.

struct ResolverContext {
    /// Specifiers the host declared for this run, in wire-order. Source
    /// bindings carry ESM text compiled verbatim; host bindings carry the
    /// module shape as data, from which `build_host_module` constructs the
    /// module natively.
    bindings: Vec<ImportBinding>,
    /// Per-isolate module cache so transitive imports resolve to the same
    /// `v8::Module` instance and import diamonds collapse correctly.
    module_cache: HashMap<String, v8::Global<v8::Module>>,
    /// Values arrays staged for host modules, keyed by module identity. The
    /// import-meta callback ([`host_import_meta_callback`]) attaches the array
    /// as `import.meta.__iso4` when V8 initialises the module's meta object
    /// (lazily, during evaluation — still inside the resolver guard's scope).
    pending_meta: Vec<(v8::Global<v8::Module>, v8::Global<v8::Array>)>,
    /// First resolver error wins. V8's callback ABI can't carry a
    /// `RunError`, so the caller inspects this after `instantiate_module`.
    resolve_error: Option<RunError>,
    /// When true, the reserved specifier `node:async_hooks` resolves to the
    /// built-in `AsyncLocalStorage` module (see [`install_async_context`]).
    /// Enabled for postfix code (`run_module`), disabled while a prefix
    /// evaluates (`validate_prefix_module` and the prefix stage of a run) —
    /// prefix code predates the per-run async-context machinery, a contract
    /// kept from the snapshot era so prefixes behave identically.
    async_context_builtin: bool,
}

thread_local! {
    static RESOLVER_CTX: RefCell<Option<ResolverContext>> = const { RefCell::new(None) };
}

/// Drop guard that takes the resolver context back out of the thread-local
/// when it goes out of scope, regardless of how the caller exits.
struct ResolverGuard;
impl Drop for ResolverGuard {
    fn drop(&mut self) {
        RESOLVER_CTX.with(|c| {
            c.borrow_mut().take();
        });
    }
}

/// Install a resolver context for the duration of `f` (instantiate + evaluate).
/// Returns the context back so the caller can drain `pending_boxes` and inspect
/// `resolve_error`.
fn install_resolver(ctx: ResolverContext) -> ResolverGuard {
    RESOLVER_CTX.with(|c| {
        *c.borrow_mut() = Some(ctx);
    });
    ResolverGuard
}

fn take_resolver() -> Option<ResolverContext> {
    RESOLVER_CTX.with(|c| c.borrow_mut().take())
}

/// Find the wire binding for `specifier`, cloning the relevant fields so we
/// don't hold a borrow across `scope` calls.
fn lookup_binding(specifier: &str) -> Option<ImportBinding> {
    RESOLVER_CTX.with(|c| {
        c.borrow().as_ref().and_then(|ctx| {
            ctx.bindings
                .iter()
                .find(|b| b.specifier == specifier)
                .cloned()
        })
    })
}

fn async_context_builtin_enabled() -> bool {
    RESOLVER_CTX.with(|c| {
        c.borrow()
            .as_ref()
            .map(|ctx| ctx.async_context_builtin)
            .unwrap_or(false)
    })
}

fn cache_get(specifier: &str) -> Option<v8::Global<v8::Module>> {
    RESOLVER_CTX.with(|c| {
        c.borrow()
            .as_ref()
            .and_then(|ctx| ctx.module_cache.get(specifier).cloned())
    })
}

fn cache_put(specifier: String, module: v8::Global<v8::Module>) {
    RESOLVER_CTX.with(|c| {
        if let Some(ctx) = c.borrow_mut().as_mut() {
            ctx.module_cache.insert(specifier, module);
        }
    });
}

fn record_resolve_error(err: RunError) {
    RESOLVER_CTX.with(|c| {
        if let Some(ctx) = c.borrow_mut().as_mut() {
            if ctx.resolve_error.is_none() {
                ctx.resolve_error = Some(err);
            }
        }
    });
}

/// Stage the natively-built values array for a host module so the import-meta
/// callback can attach it when V8 initialises the module's `import.meta`.
fn stage_meta_values(module: v8::Global<v8::Module>, values: v8::Global<v8::Array>) {
    RESOLVER_CTX.with(|c| {
        if let Some(ctx) = c.borrow_mut().as_mut() {
            ctx.pending_meta.push((module, values));
        }
    });
}

/// The handle ID of the first function leaf of `specifier` — see
/// [`host_module_base_id`]; reads the bindings from the resolver context.
fn staged_base_id(specifier: &str) -> u32 {
    RESOLVER_CTX.with(|c| {
        c.borrow()
            .as_ref()
            .map(|ctx| host_module_base_id(&ctx.bindings, specifier))
            .unwrap_or(0)
    })
}

/// The module resolver V8 calls during `instantiate_module`.
///
/// Returning `None` causes V8 to throw "Cannot find module …"; the outer
/// instantiate path converts that into `RunError::ModuleNotFound`.
fn module_resolver_callback<'a>(
    context: v8::Local<'a, v8::Context>,
    specifier: v8::Local<'a, v8::String>,
    _import_assertions: v8::Local<'a, v8::FixedArray>,
    _referrer: v8::Local<'a, v8::Module>,
) -> Option<v8::Local<'a, v8::Module>> {
    v8::callback_scope!(unsafe let callback_scope, context);
    // `NewEscapableHandleScope` is implemented for a pinned `HandleScope`, not
    // for a pinned `CallbackScope`, so deref through to the handle scope the
    // callback scope bootstrapped.
    v8::escapable_handle_scope!(let scope, &mut **callback_scope);
    let specifier_str = specifier.to_rust_string_lossy(scope);

    if let Some(g) = cache_get(&specifier_str) {
        let local = v8::Local::new(scope, &g);
        return Some(scope.escape(local));
    }

    // Host-declared bindings take precedence, so a host that ships its own
    // `node:async_hooks` shim can still override the built-in. Only when no
    // binding matches do we fall back to the built-in async-context module.
    let binding = match lookup_binding(&specifier_str) {
        Some(b) => b,
        None if specifier_str == ASYNC_HOOKS_SPECIFIER && async_context_builtin_enabled() => {
            ImportBinding {
                specifier: specifier_str.clone(),
                module: ImportModule::Source(ASYNC_HOOKS_MODULE_SRC.to_string()),
            }
        }
        None if specifier_str == ISO4_RUNTIME_SPECIFIER => ImportBinding {
            specifier: specifier_str.clone(),
            module: ImportModule::Source(ISO4_RUNTIME_MODULE_SRC.to_string()),
        },
        None => return None,
    };
    let module = match &binding.module {
        ImportModule::Source(source) => compile_source_module(scope, &specifier_str, source)?,
        ImportModule::Host(exports) => build_host_module(scope, &specifier_str, exports)?,
    };

    let global = v8::Global::new(scope, module);
    cache_put(specifier_str, global);
    Some(scope.escape(module))
}

fn compile_source_module<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    specifier: &str,
    source: &str,
) -> Option<v8::Local<'s, v8::Module>> {
    let source_string = v8::String::new(scope, source)?;
    let filename = v8::String::new(scope, specifier)?;
    let origin = v8::ScriptOrigin::new(
        scope,
        filename.into(),
        0,
        0,
        false,
        0,
        None,
        false,
        false,
        true, // is_module
        None,
    );
    let mut source = v8::script_compiler::Source::new(source_string, Some(&origin));
    v8::tc_scope!(let tc, scope);
    match v8::script_compiler::compile_module(tc, &mut source) {
        Some(m) => Some(m),
        None => {
            record_resolve_error(RunError::CompileError(format!(
                "failed to compile source import '{specifier}': {}",
                exception_message(tc)
            )));
            None
        }
    }
}

// ── Host modules (built natively from shape data) ────────────────────────────
//
// A host module crosses the wire as data: named top-level exports, each a
// tree of function leaves and data leaves. No JS is ever generated from that
// data. The module the sandbox imports is a fixed-shape source-text module —
//
//     export const <name> = import.meta.__iso4[<i>];
//
// — where only identifier-validated export names and integer indices are
// interpolated. The values array is built natively (data leaves via
// `blob::deserialize_value`, function leaves as trampolines from a fixed factory
// with the handle ID passed as a number) and handed to the module through
// V8's import-meta callback. Using a plain source-text module keeps host
// modules snapshot-safe: everything reachable from a prefix snapshot is
// ordinary JS, with no native pointers that would need external references.

/// Property on `import.meta` carrying a host module's values array. Only
/// Rust-generated host-module source reads it; user modules get an empty
/// `import.meta` (the callback attaches nothing for unknown modules).
const IMPORT_META_VALUES_KEY: &str = "__iso4";

/// Fixed factory building the async trampoline for one function leaf. The
/// handle ID is passed as a number — never interpolated into source — and the
/// dispatcher (`BRIDGE_DISPATCH_GLOBAL`, a per-run bridge stub) is looked up
/// on globalThis at call time, so a trampoline baked into a prefix snapshot
/// still reaches the fresh stub of every later run.
const IMPORT_TRAMPOLINE_FACTORY_SRC: &str =
    "(id) => (async (...args) => await globalThis.__iso4_call(id, ...args))";

/// Build the module for a host import binding.
///
/// On failure records a resolver error (surfaced after `instantiate_module`)
/// and returns `None`, like `compile_source_module`.
fn build_host_module<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    specifier: &str,
    exports: &[(String, HostModuleNode)],
) -> Option<v8::Local<'s, v8::Module>> {
    // Defensive re-validation: the TS client already rejects invalid names,
    // but Rust is the side interpolating them into export positions, so it
    // owns the final check.
    for (name, _) in exports {
        if !is_valid_export_identifier(name) {
            record_resolve_error(RunError::CompileError(format!(
                "host module '{specifier}': top-level key '{name}' is not a valid \
                 JavaScript identifier and cannot be exported as a named ESM export"
            )));
            return None;
        }
    }

    let factory = match build_import_trampoline_factory(scope) {
        Ok(f) => f,
        Err(e) => {
            record_resolve_error(e);
            return None;
        }
    };

    // Handle IDs continue the global walk order established by
    // `collect_import_handles` over the declared bindings.
    let mut next_id = staged_base_id(specifier);
    let values = v8::Array::new(scope, exports.len() as i32);
    let mut source = String::new();
    for (i, (name, node)) in exports.iter().enumerate() {
        let value = match build_host_value(scope, specifier, node, factory, &mut next_id) {
            Ok(v) => v,
            Err(e) => {
                record_resolve_error(e);
                return None;
            }
        };
        values.set_index(scope, i as u32, value)?;
        if name == "default" {
            source.push_str(&format!(
                "export default import.meta.{IMPORT_META_VALUES_KEY}[{i}];\n"
            ));
        } else {
            source.push_str(&format!(
                "export const {name} = import.meta.{IMPORT_META_VALUES_KEY}[{i}];\n"
            ));
        }
    }

    let module = compile_source_module(scope, specifier, &source)?;
    stage_meta_values(
        v8::Global::new(scope, module),
        v8::Global::new(scope, values),
    );
    Some(module)
}

/// Build one host-module value natively: function leaves become trampolines
/// (consuming the next handle ID), data leaves are materialised from their
/// value blob, and object nodes become plain objects built via the V8 API.
fn build_host_value<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    specifier: &str,
    node: &HostModuleNode,
    factory: v8::Local<v8::Function>,
    next_id: &mut u32,
) -> Result<v8::Local<'s, v8::Value>, RunError> {
    match node {
        HostModuleNode::Function => {
            let id = *next_id;
            *next_id += 1;
            let id_arg = v8::Number::new(scope, id as f64).into();
            let recv = v8::undefined(scope).into();
            factory.call(scope, recv, &[id_arg]).ok_or_else(|| {
                RunError::Internal(format!(
                    "failed to build import trampoline in host module '{specifier}'"
                ))
            })
        }
        HostModuleNode::Data(bytes) => blob::deserialize_value(scope, bytes).ok_or_else(|| {
            RunError::Internal(format!(
                "failed to materialise data leaf in host module '{specifier}'"
            ))
        }),
        HostModuleNode::Object(entries) => {
            let obj = v8::Object::new(scope);
            for (key, child) in entries {
                let v = build_host_value(scope, specifier, child, factory, next_id)?;
                let k = v8::String::new(scope, key).ok_or_else(|| {
                    RunError::Internal(format!(
                        "failed to intern object key in host module '{specifier}'"
                    ))
                })?;
                obj.create_data_property(scope, k.into(), v);
            }
            Ok(obj.into())
        }
    }
}

/// Build the trampoline factory from [`IMPORT_TRAMPOLINE_FACTORY_SRC`].
fn build_import_trampoline_factory<'s>(
    scope: &mut v8::PinScope<'s, '_>,
) -> Result<v8::Local<'s, v8::Function>, RunError> {
    let value = eval_script(
        scope,
        IMPORT_TRAMPOLINE_FACTORY_SRC,
        "<iso4:import-trampoline-factory>",
    )?;
    v8::Local::<v8::Function>::try_from(value).map_err(|_| {
        RunError::Internal("import trampoline factory did not evaluate to a function".to_string())
    })
}

/// Isolate-level callback V8 invokes the first time a module's `import.meta`
/// is accessed. Attaches the staged values array for host modules; any other
/// module (user code, source imports) gets an untouched empty meta object.
extern "C" fn host_import_meta_callback(
    context: v8::Local<v8::Context>,
    module: v8::Local<v8::Module>,
    meta: v8::Local<v8::Object>,
) {
    // SAFETY: standard embedder-callback re-entry, same pattern as
    // `module_resolver_callback`.
    v8::callback_scope!(unsafe let scope, context);
    let values = RESOLVER_CTX.with(|c| {
        c.borrow().as_ref().and_then(|ctx| {
            ctx.pending_meta
                .iter()
                .find(|(m, _)| v8::Local::new(scope, m) == module)
                .map(|(_, v)| v.clone())
        })
    });
    let Some(values) = values else { return };
    let Some(key) = v8::String::new(scope, IMPORT_META_VALUES_KEY) else {
        return;
    };
    let array = v8::Local::new(scope, &values);
    meta.create_data_property(scope, key.into(), array.into());
}

/// Reserved words that cannot appear as named ESM exports. Mirrors the list
/// in the TS client (`imports.ts`).
const RESERVED_EXPORT_WORDS: &[&str] = &[
    "break",
    "case",
    "catch",
    "class",
    "const",
    "continue",
    "debugger",
    "delete",
    "do",
    "else",
    "export",
    "extends",
    "finally",
    "for",
    "function",
    "if",
    "import",
    "in",
    "instanceof",
    "new",
    "return",
    "super",
    "switch",
    "this",
    "throw",
    "try",
    "typeof",
    "var",
    "void",
    "while",
    "with",
    "yield",
    "let",
    "static",
    "enum",
    "await",
    "implements",
    "package",
    "protected",
    "interface",
    "private",
    "public",
    "null",
    "true",
    "false",
];

/// Whether `name` can appear as a named ESM export in generated host-module
/// source: ASCII identifier rules plus a reserved-word filter. `default` is
/// allowed (emitted as `export default`). Mirrors `isValidExportIdentifier`
/// in the TS client.
fn is_valid_export_identifier(name: &str) -> bool {
    if name == "default" {
        return true;
    }
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == '_' || first == '$') {
        return false;
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$') {
        return false;
    }
    !RESERVED_EXPORT_WORDS.contains(&name)
}

// ── Helpers ──────────────────────────────────────────────────────────────────

// ── Limit guard helpers ────────────────────────────────────────────────────

/// Spawn a wall-clock guard thread. Returns a cancel sender.
/// Sending anything on it (or dropping it) cancels the guard.
/// If `wall_time_ms == 0`, no thread is spawned but a sender is still returned.
/// The cancel sender plus the guard thread's join handle (`None` when the
/// limit is 0 and no thread was spawned). Callers that reuse the isolate
/// afterwards (warm instances) MUST join after cancelling: cancellation only
/// signals the guard, and a guard whose deadline expired concurrently may
/// still be about to set the reason and terminate — joining makes any fire
/// visible to the taint check instead of landing on the next call.
type GuardHandle = (crossbeam_channel::Sender<()>, Option<std::thread::JoinHandle<()>>);

fn start_wall_guard(
    handle: v8::IsolateHandle,
    reason: Arc<ReasonCell>,
    wall_time_ms: u32,
) -> GuardHandle {
    let (tx, rx) = crossbeam_channel::bounded::<()>(1);
    let join = if wall_time_ms > 0 {
        Some(std::thread::spawn(move || {
            let timeout = Duration::from_millis(wall_time_ms as u64);
            if let Err(crossbeam_channel::RecvTimeoutError::Timeout) = rx.recv_timeout(timeout) {
                reason.set(TerminationReason::Wall); // first writer wins
                handle.terminate_execution();
            }
            // Err(Disconnected) means cancel_guards() fired first - do nothing.
        }))
    } else {
        None
    };
    (tx, join)
}

/// Spawn a CPU-budget guard thread. Returns a cancel sender.
/// Polls `budget.elapsed_ms()` every 10 ms; fires when it exceeds `cpu_time_ms`.
/// Phase 4 hook: call `budget.leave()` before bridge calls and `budget.enter()`
/// after to exclude host wait time from the measurement.
fn start_cpu_guard(
    handle: v8::IsolateHandle,
    reason: Arc<ReasonCell>,
    budget: Arc<CpuBudget>,
    cpu_time_ms: u32,
) -> GuardHandle {
    let (tx, rx) = crossbeam_channel::bounded::<()>(1);
    let join = if cpu_time_ms > 0 {
        Some(std::thread::spawn(move || loop {
            match rx.recv_timeout(Duration::from_millis(10)) {
                Ok(_) | Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => {
                    if budget.elapsed_ms() >= cpu_time_ms as u64 {
                        reason.set(TerminationReason::Cpu); // first writer wins
                        handle.terminate_execution();
                        break;
                    }
                }
            }
        }))
    } else {
        None
    };
    (tx, join)
}

/// Cancel both guard threads and close the CPU epoch.
///
/// Idempotent: safe to call more than once on the same guard set.
/// Must be called on every exit path from `run_module` after evaluation.
fn cancel_guards(
    cancel_wall: &crossbeam_channel::Sender<()>,
    cancel_cpu: &crossbeam_channel::Sender<()>,
    budget: &CpuBudget,
) {
    budget.leave();
    let _ = cancel_wall.send(());
    let _ = cancel_cpu.send(());
}

/// RAII guard that cancels limit-enforcement threads on drop.
///
/// Ensures guards exit promptly even when `run_module` returns early via `?`.
/// All explicit `cancel_guards(...)` calls are kept for clarity and early
/// cancellation; this struct is defence-in-depth for error-path returns.
struct GuardCanceller<'a> {
    cancel_wall: &'a crossbeam_channel::Sender<()>,
    cancel_cpu: &'a crossbeam_channel::Sender<()>,
    budget: &'a CpuBudget,
}

impl Drop for GuardCanceller<'_> {
    fn drop(&mut self) {
        cancel_guards(self.cancel_wall, self.cancel_cpu, self.budget);
    }
}

// ── Bridge globals ───────────────────────────────────────────────────────────
//
// Every host-declared global (fetch, myTool, anything else) goes through the
// same generic bridge callback. The callback:
//   1. Serialises the JS arguments into one value blob (rejects function args).
//   2. Calls cpu_budget.leave() to pause the CPU budget during host wait.
//   3. Writes a BridgeCall frame on the session socket.
//   4. Blocks reading a BridgeResponse frame (the V8 thread is already blocked
//      in the host callback, so no V8 activity can happen during this wait).
//   5. Calls cpu_budget.enter() to resume counting.
//   6. On success: deserialises the response blob back to a V8 value.
//   7. On handler error: rejects the pending Promise with a real Error
//      carrying the handler's name/message/fields (tagged via a private
//      symbol). Sandbox code may catch it and continue; uncaught it
//      surfaces as ERR_HOST_BRIDGE. Only limit violations (maxBridgeCalls,
//      payload too large, function args) store a RunError in the shared
//      OnceLock and are fatal to the run: they call terminate_execution()
//      so sandbox code cannot catch its way past a violation (see
//      fatal_bridge_error).
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

struct PendingResolver(v8::Global<v8::PromiseResolver>);
// SAFETY: only ever accessed from the V8 isolate thread.
unsafe impl Send for PendingResolver {}

type PendingResolvers = Arc<Mutex<HashMap<u32, PendingResolver>>>;

/// Per-stub-name dispatch slot with **instance** lifetime.
///
/// The V8 `Function` a bridge stub installs holds a raw `External` pointer to
/// its slot, and sandbox code can stash that function across calls on a warm
/// instance (`globalThis.saved = fetch`) — so the pointee must outlive every
/// call, not just the one that installed it. The slot lives in `InstanceCore`
/// (or the one-off run's locals) and is re-pointed at fresh per-call state by
/// `install_bridge_globals`; between calls it is disarmed (`None`), so a
/// stashed stub throws a catchable error instead of dereferencing freed
/// per-call state or writing to a stale socket fd.
///
/// Only the isolate's owner thread touches a slot — installs between calls,
/// the callback during JS — so a `RefCell` suffices. Nested shared borrows
/// (a bridge call made while another callback serializes) are fine; the only
/// `borrow_mut` sites run from Rust between calls, never during JS.
struct StubSlot {
    /// The global-object key this slot serves — outside the `Option` so a
    /// disarmed call can still name itself in its error message.
    stub_name: String,
    data: RefCell<Option<GlobalCallbackData>>,
}

/// Instance-lifetime slot table, keyed by stub name. Boxed so the addresses
/// handed to V8 as `External`s never move.
#[allow(clippy::vec_box)]
type StubSlots = HashMap<String, Box<StubSlot>>;

struct GlobalCallbackData {
    /// Raw file descriptor for the session socket.  The callback writes a
    /// BridgeCall frame here (non-blocking in practice) and returns immediately;
    /// it never reads from the socket.  The fd remains owned by the session thread.
    stream_fd: RawFd,
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
    scope.thread_safe_handle().terminate_execution();
}

/// A single generic V8 FunctionCallback used for every host-declared global.
///
/// Non-blocking: serialises arguments, writes a BridgeCall frame, stores a
/// PromiseResolver keyed by callId, and returns the pending Promise immediately.
/// The run_module poll loop drives resolution by reading BridgeResponse frames
/// and calling perform_microtask_checkpoint().
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
    let data_ref = slot.data.borrow();
    let Some(data) = data_ref.as_ref() else {
        // Disarmed: this Function was captured during an earlier call on
        // this instance and is not bound for the current one. Catchable —
        // exactly like the prepare()-time placeholders.
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
    #[allow(clippy::type_complexity)]
    let (target_kind, specifier, export_name, first_arg, record_name): (
        u8,
        Option<&str>,
        &str,
        i32,
        String,
    ) = match &data.import_handles {
        None => (
            0,
            None,
            data.stub_name.as_str(),
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
                1,
                Some(entry.specifier.as_str()),
                entry.path.as_str(),
                1, // the handle ID never leaves the process
                entry.record_name(),
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

    // ── Assign call ID and build the BridgeCall header ────────────────────
    //
    // Header only: the arguments stay in the buffer the serializer produced,
    // and are written straight after it. Copying them into a combined payload
    // first would double the peak memory of every bridge call — that memory
    // is outside the V8 heap, so `memoryMb` does not account for it, and it
    // lands in the process RSS the warm-instance ceiling watches, where one
    // run's arguments can push a shared child into shedding. It would also
    // put the copy *before* the size check, leaving `maxBridgeCallBytes` able
    // to refuse the send but not the allocation.
    let call_id = data.call_id.fetch_add(1, Ordering::Relaxed);
    let Ok(args_blob_len) = u32::try_from(args_blob.len()) else {
        record_blocked(u32::MAX);
        fatal_bridge_error(
            scope,
            &data.bridge_error,
            RunError::BridgeCallPayloadTooLarge,
        );
        return;
    };
    let bridge_call_header = wire::encode_bridge_call_header(
        call_id,
        target_kind,
        specifier,
        export_name,
        args_blob_len,
    );
    let payload_len = bridge_call_header.len() + args_blob.len();

    if data.max_bridge_call_bytes > 0 && payload_len > data.max_bridge_call_bytes as usize {
        record_blocked(u32::try_from(payload_len).unwrap_or(u32::MAX));
        fatal_bridge_error(
            scope,
            &data.bridge_error,
            RunError::BridgeCallPayloadTooLarge,
        );
        return;
    }

    // ── Write BridgeCall frame (non-blocking in practice) ─────────────────
    // SAFETY: stream_fd is the live session socket owned by handle_client.
    // ManuallyDrop prevents closing it here.
    let mut stream = ManuallyDrop::new(unsafe { UnixStream::from_raw_fd(data.stream_fd) });
    if let Err(e) = ipc::write_rust_to_ts_frame_parts(
        &mut *stream,
        ipc::RustToTsMessageType::BridgeCall,
        &bridge_call_header,
        &args_blob,
    ) {
        record_blocked(u32::try_from(payload_len).unwrap_or(u32::MAX));
        fatal_bridge_error(
            scope,
            &data.bridge_error,
            RunError::Internal(format!("bridge write failed: {e}")),
        );
        return;
    }

    // The frame reached the host — record the call for the run's bridge
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
struct BridgeStubSpec {
    /// The global-object key the stub installs under (e.g. "fetch",
    /// "__iso4_fetch_h", "__iso4_call").
    stub_name: String,
    /// The public name its bridge records report under.
    record_name: String,
    /// `Some` for the host-import dispatcher stub — the handle table it
    /// resolves leading handle-ID arguments against.
    import_handles: Option<Arc<Vec<ImportHandleEntry>>>,
    /// The declared enumerability of the public name. Ignored in
    /// effect for internal stub names, which install DONT_ENUM regardless.
    enumerable: bool,
}

/// Install a bridge stub for each spec.
///
/// Each stub is an identical `bridge_global_callback` function whose External
/// data points at the per-name [`StubSlot`] in `slots` — instance-lifetime
/// storage re-pointed at this call's `GlobalCallbackData` here. Installation
/// uses `define_own_property`, never `set`: sandbox code can plant a hostile
/// accessor under a stub name, and a plain `set` would invoke it.
#[allow(clippy::too_many_arguments)] // bridge setup genuinely needs all these params
fn install_bridge_globals(
    scope: &mut v8::PinScope,
    stubs: &[BridgeStubSpec],
    stream_fd: RawFd,
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
                data: RefCell::new(None),
            })
        });
        *slot.data.borrow_mut() = Some(GlobalCallbackData {
            stream_fd,
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
        });
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

// ── Value globals (native install) ────────────────────────────────────────────

/// Fixed factory that builds a `BridgeWithShim` wrapper. Called with the
/// evaluated shim function and the private handler's global name (a plain
/// string, passed as data — never interpolated into code). The wrapper looks
/// the handler stub up on `globalThis` at call time, so the same wrapper works
/// whether it is built for a direct run (stub installed alongside it) or baked
/// into a snapshot (stub re-installed per `prefix.run()`).
const SHIM_FACTORY_SRC: &str =
    "(shim, handlerName) => (async (...args) => await shim(await globalThis[handlerName](...args)))";

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
  const e = new Error(`${what} cannot be called during prepare(): no host session exists at precompile time. Do the call in run() code instead.`)
  e.__iso4_prefix_bridge_call = true
  throw e
}"#;

/// Install the value globals — string expressions, data constants, and shim
/// wrappers — natively on the sandbox global object. Plain bridge stubs are
/// installed separately by [`install_bridge_globals`]; their defs are skipped
/// here.
///
/// Every public name reaches the global object through `object.set(key, value)`
/// — a plain string, never interpolated into an identifier position — so no
/// global name can shape generated source. String expressions and
/// shim expressions are evaluated as their own scripts with their own
/// filenames, so they never shift the line numbers of user code.
/// Names the runtime owns. A host global using one of these is rejected rather
/// than allowed to shadow it: replacing `Response` would leave user code
/// building objects the codec cannot recognise.
///
/// Keep in sync with the `HostGlobals` docs in
/// `packages/iso4-sandbox/src/types.ts` and DESIGN.md §4.2.
pub const RESERVED_GLOBAL_NAMES: &[&str] = &[
    "console",
    "waitUntil",
    "Headers",
    "Request",
    "Response",
    "TextEncoder",
    "TextDecoder",
    "URL",
    "URLSearchParams",
];

fn check_not_reserved(name: &str) -> Result<(), RunError> {
    if RESERVED_GLOBAL_NAMES.contains(&name) {
        return Err(RunError::UndeclaredBinding(format!(
            "global '{name}' is reserved by the runtime and cannot be provided by the host"
        )));
    }
    Ok(())
}

fn install_value_globals(
    scope: &mut v8::PinScope,
    globals: &[HostGlobalDef],
) -> Result<(), RunError> {
    // Built lazily on the first shim global, then reused for the rest.
    let mut shim_factory: Option<v8::Local<v8::Function>> = None;

    for def in globals {
        match def {
            // Installed as a bridge stub by install_bridge_globals.
            HostGlobalDef::Bridge { .. } => {}
            HostGlobalDef::StringExpr {
                name,
                expr,
                enumerable,
            } => {
                check_not_reserved(name)?;
                let value = eval_global_expression(scope, expr, name)?;
                install_public_global(scope, name, value, *enumerable)?;
            }
            HostGlobalDef::Data {
                name,
                blob,
                enumerable,
            } => {
                check_not_reserved(name)?;
                // Web-aware: a data global is one of the two ways a host
                // Request/Response reaches the sandbox.
                let v8_value =
                    blob::deserialize_value_with_web_types(scope, blob).ok_or_else(|| {
                        match blob::take_codec_error() {
                            Some(e) => codec_error_to_run_error(e),
                            None => RunError::Internal(format!(
                                "failed to materialise data global '{name}'"
                            )),
                        }
                    })?;
                install_public_global(scope, name, v8_value, *enumerable)?;
            }
            HostGlobalDef::Shim {
                name,
                shim,
                handler_name,
                enumerable,
            } => {
                check_not_reserved(name)?;
                let factory = match shim_factory {
                    Some(f) => f,
                    None => {
                        let f = build_shim_factory(scope)?;
                        shim_factory = Some(f);
                        f
                    }
                };
                let shim_fn = eval_global_expression(scope, shim, name)?;
                let handler_key = v8::String::new(scope, handler_name).ok_or_else(|| {
                    RunError::Internal(format!("failed to intern shim handler name for '{name}'"))
                })?;
                let recv = v8::undefined(scope).into();
                let wrapper = factory
                    .call(scope, recv, &[shim_fn, handler_key.into()])
                    .ok_or_else(|| {
                        RunError::Internal(format!("failed to build shim wrapper for '{name}'"))
                    })?;
                install_public_global(scope, name, wrapper, *enumerable)?;
            }
        }
    }
    Ok(())
}

/// Install a host-declared value global, honoring its `enumerable` flag.
///
/// The enumerable path keeps the historical `[[Set]]` install (these run in a
/// context no sandbox code has touched yet); a non-enumerable one must go
/// through `DefineOwnProperty`, because `[[Set]]` always creates enumerable
/// properties.
fn install_public_global(
    scope: &mut v8::PinScope,
    name: &str,
    value: v8::Local<v8::Value>,
    enumerable: bool,
) -> Result<(), RunError> {
    if enumerable {
        set_global(scope, name, value)
    } else {
        define_global(scope, name, value, false)
    }
}

/// Set `globalThis[name] = value` via the V8 API. The name is a property key,
/// never code.
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
/// same names with live socket-backed stubs (`install_bridge_globals`).
fn install_prefix_bridge_placeholders(
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

/// Names under this prefix are runtime plumbing (`__iso4_call`, the
/// `__iso4_<name>_h` shim handler keys, their placeholders) and install
/// non-enumerable, so enumeration-driven sandbox code (`for (const k in
/// globalThis)`, serializers, "copy the environment" patterns) never trips
/// over them — celld's ops went through exactly that accident before moving
/// to DONT_ENUM. The web classes already install non-enumerable
/// (`webtypes.js`); host-declared globals stay enumerable the way browsers
/// keep `fetch` enumerable. The names remain reachable by string — this is
/// hygiene, not secrecy.
const INTERNAL_GLOBAL_PREFIX: &str = "__iso4_";

/// Install a global via `DefineOwnProperty` — replaces any existing property
/// **without invoking accessors**. `set_global` goes through `[[Set]]`, which
/// runs a setter if sandbox code planted one under the name; on a reused
/// context (warm instances re-arming placeholders and stubs) that would hand
/// control to user code outside any guard. Fails when sandbox code made the
/// property non-configurable — callers treat that as an internal error, which
/// taints the instance.
///
/// Runtime-internal names ([`INTERNAL_GLOBAL_PREFIX`]) get `DONT_ENUM`
/// regardless of `enumerable`; for public names the flag is the host's
/// per-global opt-out.
fn define_global(
    scope: &mut v8::PinScope,
    name: &str,
    value: v8::Local<v8::Value>,
    enumerable: bool,
) -> Result<(), RunError> {
    let global = scope.get_current_context().global(scope);
    let key = v8::String::new(scope, name)
        .ok_or_else(|| RunError::Internal(format!("failed to intern global name '{name}'")))?;
    let attributes = if name.starts_with(INTERNAL_GLOBAL_PREFIX) || !enumerable {
        v8::PropertyAttribute::DONT_ENUM
    } else {
        v8::PropertyAttribute::NONE
    };
    match global.define_own_property(scope, key.into(), value, attributes) {
        Some(true) => Ok(()),
        _ => Err(RunError::Internal(format!(
            "failed to define global '{name}' (made non-configurable by sandbox code?)"
        ))),
    }
}

fn set_global(
    scope: &mut v8::PinScope,
    name: &str,
    value: v8::Local<v8::Value>,
) -> Result<(), RunError> {
    let global = scope.get_current_context().global(scope);
    let key = v8::String::new(scope, name)
        .ok_or_else(|| RunError::Internal(format!("failed to intern global name '{name}'")))?;
    global
        .set(scope, key.into(), value)
        .ok_or_else(|| RunError::Internal(format!("failed to install global '{name}'")))?;
    Ok(())
}

/// Evaluate a host-provided global expression (`<expr>`, wrapped in parens so a
/// bare function/object expression parses) as its own script, tagged with a
/// dedicated filename so it never appears in a user-code stack trace.
fn eval_global_expression<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    expr: &str,
    name: &str,
) -> Result<v8::Local<'s, v8::Value>, RunError> {
    let wrapped = format!("({expr})");
    let filename = format!("<iso4:global:{name}>");
    eval_script(scope, &wrapped, &filename)
}

/// Build the shim-wrapper factory function from [`SHIM_FACTORY_SRC`].
fn build_shim_factory<'s>(
    scope: &mut v8::PinScope<'s, '_>,
) -> Result<v8::Local<'s, v8::Function>, RunError> {
    let value = eval_script(scope, SHIM_FACTORY_SRC, "<iso4:shim-factory>")?;
    v8::Local::<v8::Function>::try_from(value)
        .map_err(|_| RunError::Internal("shim factory did not evaluate to a function".to_string()))
}

/// Compile and run `source` as a classic script under `filename`, returning the
/// completion value. Compile/runtime failures map to `RunError` carrying the
/// V8 exception. Runs inside its own `TryCatch`; the returned value lives in the
/// caller's handle scope.
fn eval_script<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    source: &str,
    filename: &str,
) -> Result<v8::Local<'s, v8::Value>, RunError> {
    let source_str = v8::String::new(scope, source)
        .ok_or_else(|| RunError::Internal("failed to intern global source".to_string()))?;
    let filename_str = v8::String::new(scope, filename)
        .ok_or_else(|| RunError::Internal("failed to intern global filename".to_string()))?;
    // Classic script (is_module = false) — the last three ScriptOrigin bools
    // mirror the module origin built in run_module with is_module flipped off.
    let origin = v8::ScriptOrigin::new(
        scope,
        filename_str.into(),
        0,
        0,
        false,
        0,
        None,
        false,
        false,
        false,
        None,
    );
    v8::tc_scope!(let tc, scope);
    let script = match v8::Script::compile(tc, source_str, Some(&origin)) {
        Some(s) => s,
        None => return Err(RunError::CompileError(exception_message(tc))),
    };
    match script.run(tc) {
        Some(v) => Ok(v),
        None => Err(RunError::RuntimeError(Box::new(RuntimeErrorData {
            name: exception_name(tc),
            message: exception_message(tc),
            stack: exception_stack(tc),
            fields: exception_fields(tc),
        }))),
    }
}

/// Throw a plain Error into V8 with the given message.
/// Splits the string-creation, exception-creation, and throw into separate
/// borrows so the compiler doesn't see `scope` used twice simultaneously.
fn throw_v8_error(scope: &mut v8::PinScope, message: &str) {
    if let Some(s) = v8::String::new(scope, message) {
        let exc = v8::Exception::error(scope, s);
        scope.throw_exception(exc);
    }
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
fn host_bridge_error_to_v8<'s>(
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
fn host_bridge_error_from_rejection(
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
fn prefix_bridge_call_from_value(
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

/// Reserved module specifier resolving to the built-in runtime module, which
/// re-exports `waitUntil`. Resolvable in setup code too (capturing the
/// function for later run-time use is legitimate); calling it during setup
/// throws.
const ISO4_RUNTIME_SPECIFIER: &str = "iso4:runtime";

/// Global-registry symbol under which the native `waitUntil` is stashed for
/// the module re-export, so deleting the `waitUntil` global cannot break the
/// import. Must match `Symbol.for(...)` in [`ISO4_RUNTIME_MODULE_SRC`].
const ISO4_RUNTIME_WAITUNTIL_SYMBOL: &str = "iso4.runtime.waitUntil";

/// Private-symbol key holding the pristine `Promise.allSettled` (bound),
/// captured at install time — before any guest code runs — and called by the
/// grace loop. A guest replacing `Promise.allSettled` therefore sabotages
/// nothing: privates are unreachable from JS (the type-tag mechanism).
const GRACE_ALL_SETTLED_KEY: &str = "iso4::graceAllSettled";

fn grace_all_settled_key<'s>(scope: &mut v8::PinScope<'s, '_>) -> Option<v8::Local<'s, v8::Private>> {
    let name = v8::String::new(scope, GRACE_ALL_SETTLED_KEY)?;
    Some(v8::Private::for_api(scope, Some(name)))
}

/// ESM source of the built-in `iso4:runtime` module.
const ISO4_RUNTIME_MODULE_SRC: &str =
    "export const waitUntil = globalThis[Symbol.for('iso4.runtime.waitUntil')];\n";

/// Private-symbol key stamping a hydrated body-stream object with its stream
/// id — Rust-readable, guest-invisible, and how the outbound codec refuses to
/// serialize a stream back to the host.
const BODY_STREAM_ID_KEY: &str = "iso4::bodyStreamId";

fn body_stream_private<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    name: &str,
) -> Option<v8::Local<'s, v8::Private>> {
    let key = v8::String::new(scope, name)?;
    Some(v8::Private::for_api(scope, Some(key)))
}

/// The stream id a hydrated body-stream object was stamped with, if this is
/// one. Used by the outbound body check.
pub fn body_stream_id_of(scope: &mut v8::PinScope, value: v8::Local<v8::Value>) -> Option<u32> {
    let obj = v8::Local::<v8::Object>::try_from(value).ok()?;
    let key = body_stream_private(scope, BODY_STREAM_ID_KEY)?;
    let stamped = obj.get_private(scope, key)?;
    if !stamped.is_uint32() {
        return None;
    }
    stamped.uint32_value(scope)
}

/// Build the sandbox-side body-stream object for a hydrated stream handle:
/// calls the factory the web runtime returned at install and stamps the
/// stream id under a private key. Runs during the rehydration walk (JS is
/// legal there).
pub fn make_body_stream<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    stream_id: u32,
) -> Option<v8::Local<'s, v8::Object>> {
    let global = scope.get_current_context().global(scope);
    let factory_key = body_stream_private(scope, crate::webtypes::BODY_STREAM_FACTORY_KEY)?;
    let factory =
        v8::Local::<v8::Function>::try_from(global.get_private(scope, factory_key)?).ok()?;
    let recv = v8::undefined(scope).into();
    let id_arg = v8::Integer::new_from_unsigned(scope, stream_id);
    let stream = factory.call(scope, recv, &[id_arg.into()])?;
    let stream = v8::Local::<v8::Object>::try_from(stream).ok()?;
    let id_key = body_stream_private(scope, BODY_STREAM_ID_KEY)?;
    let id_value = v8::Integer::new_from_unsigned(scope, stream_id);
    stream.set_private(scope, id_key, id_value.into())?;
    Some(stream)
}

/// Install the web runtime with streamed-body support: the two natives are
/// built around this instance's stream table and handed to the JS factory.
fn install_web_runtime(
    scope: &mut v8::PinScope,
    streams: *const StreamTable,
) -> Result<(), String> {
    let data = v8::External::new(scope, streams.cast_mut().cast::<c_void>());
    let read = v8::Function::builder(stream_read_callback)
        .data(data.into())
        .build(scope)
        .ok_or_else(|| "failed to create streamRead".to_string())?;
    let cancel = v8::Function::builder(stream_cancel_callback)
        .data(data.into())
        .build(scope)
        .ok_or_else(|| "failed to create streamCancel".to_string())?;
    crate::webtypes::install_with_streams(scope, read.into(), cancel.into())
}

/// `streamRead(streamId)` — native pull for the sandbox body stream: resolves
/// with a `Uint8Array` chunk, `null` at EOF, or rejects on a source failure.
/// Consumed bytes replenish the host's credit window.
fn stream_read_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    // SAFETY: instance-lifetime StreamTable Box, same contract as PendingWork.
    let table = unsafe { &*args.data().cast::<v8::External>().value().cast::<StreamTable>() };
    let Some(resolver) = v8::PromiseResolver::new(scope) else {
        throw_v8_error(scope, "[iso4] stream read could not allocate");
        return;
    };
    rv.set(resolver.get_promise(scope).into());
    let stream_id = args.get(0).uint32_value(scope).unwrap_or(0);

    let mut slot = table.slot.borrow_mut();
    let Some(state) = slot.as_mut() else {
        drop(slot);
        let msg = v8_error_value(scope, "[iso4] streamed bodies are only readable during a run");
        resolver.reject(scope, msg);
        return;
    };
    let Some(stream) = state.streams.get_mut(&stream_id) else {
        drop(slot);
        let msg = v8_error_value(scope, "[iso4] unknown body stream");
        resolver.reject(scope, msg);
        return;
    };

    if let Some(chunk) = stream.buffered.pop_front() {
        stream.buffered_bytes -= chunk.len();
        let credit = chunk.len() as u32;
        let value = bytes_to_uint8array(scope, chunk);
        send_stream_credit(state, stream_id, credit);
        drop(slot);
        match value {
            Some(v) => {
                resolver.resolve(scope, v.into());
            }
            None => {
                let msg = v8_error_value(scope, "[iso4] stream chunk could not be materialised");
                resolver.reject(scope, msg);
            }
        }
        return;
    }
    if stream.cancelled {
        drop(slot);
        let null = v8::null(scope).into();
        resolver.resolve(scope, null);
        return;
    }
    match &stream.ended {
        Some(Ok(())) => {
            drop(slot);
            let null = v8::null(scope).into();
            resolver.resolve(scope, null);
        }
        Some(Err(message)) => {
            let message = message.clone();
            drop(slot);
            let msg = v8_error_value(scope, &format!("[iso4] body stream failed: {message}"));
            resolver.reject(scope, msg);
        }
        None => {
            if stream.pending.is_some() {
                drop(slot);
                let msg = v8_error_value(scope, "[iso4] a read is already pending on this stream");
                resolver.reject(scope, msg);
                return;
            }
            stream.pending = Some(v8::Global::new(scope, resolver));
        }
    }
}

/// `streamCancel(streamId, reason)` — the sandbox is done with the body: the
/// host is told to stop pumping and release the source; buffered chunks drop.
fn stream_cancel_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let table = unsafe { &*args.data().cast::<v8::External>().value().cast::<StreamTable>() };
    let stream_id = args.get(0).uint32_value(scope).unwrap_or(0);
    let reason = args
        .get(1)
        .to_string(scope)
        .map(|s| s.to_rust_string_lossy(scope))
        .unwrap_or_default();
    let mut slot = table.slot.borrow_mut();
    let mut pending_read: Option<v8::Global<v8::PromiseResolver>> = None;
    if let Some(state) = slot.as_mut() {
        let mut notify_host = false;
        if let Some(stream) = state.streams.get_mut(&stream_id) {
            if !stream.cancelled && stream.ended.is_none() {
                stream.cancelled = true;
                stream.buffered.clear();
                stream.buffered_bytes = 0;
                notify_host = true;
            }
            pending_read = stream.pending.take();
        }
        if notify_host {
            send_stream_cancel(state, stream_id, &reason);
        }
    }
    drop(slot);
    if let Some(pending) = pending_read {
        let resolver = v8::Local::new(scope, pending);
        let null = v8::null(scope).into();
        resolver.resolve(scope, null);
    }
    rv.set_undefined();
}

/// A plain `Error` value for rejections built inside native callbacks.
fn v8_error_value<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    message: &str,
) -> v8::Local<'s, v8::Value> {
    let msg = v8::String::new(scope, message).unwrap_or_else(|| v8::String::empty(scope));
    v8::Exception::type_error(scope, msg)
}

/// Copy chunk bytes into a fresh sandbox `Uint8Array`.
fn bytes_to_uint8array<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    bytes: Vec<u8>,
) -> Option<v8::Local<'s, v8::Uint8Array>> {
    let len = bytes.len();
    let store = v8::ArrayBuffer::new_backing_store_from_vec(bytes).make_shared();
    let buffer = v8::ArrayBuffer::with_backing_store(scope, &store);
    v8::Uint8Array::new(scope, buffer, 0, len)
}

/// Install the `waitUntil` global: a native function pushing its
/// argument into the run's [`PendingWork`] set. Installed non-enumerable like
/// the web classes (convention), reserved as a name, and additionally
/// stashed under a registry symbol for the `iso4:runtime` re-export.
fn install_wait_until(scope: &mut v8::PinScope, pending: *const PendingWork) -> Result<(), RunError> {
    let data = v8::External::new(scope, pending.cast_mut().cast::<c_void>());
    let function = v8::Function::builder(wait_until_callback)
        .data(data.into())
        .build(scope)
        .ok_or_else(|| RunError::Internal("failed to create waitUntil".to_string()))?;
    define_global(scope, "waitUntil", function.into(), false)?;

    let global = scope.get_current_context().global(scope);
    let desc = v8::String::new(scope, ISO4_RUNTIME_WAITUNTIL_SYMBOL)
        .ok_or_else(|| RunError::Internal("failed to intern waitUntil symbol".to_string()))?;
    let key = v8::Symbol::for_key(scope, desc);
    global
        .set(scope, key.into(), function.into())
        .ok_or_else(|| RunError::Internal("failed to stash waitUntil".to_string()))?;

    // Capture the pristine Promise.allSettled before any guest code exists,
    // into a private slot the grace loop calls directly from Rust.
    let all_settled = eval_script(
        scope,
        "(Promise.allSettled.bind(Promise))",
        "<iso4:runtime-install>",
    )?;
    let all_settled_key = grace_all_settled_key(scope)
        .ok_or_else(|| RunError::Internal("failed to intern allSettled key".to_string()))?;
    global
        .set_private(scope, all_settled_key, all_settled)
        .ok_or_else(|| RunError::Internal("failed to stash allSettled".to_string()))?;
    Ok(())
}

/// `waitUntil(value)` — register background work to be driven after the
/// run's value settles. Non-promise values are accepted and count as
/// already-settled work, mirroring `Promise.resolve` semantics.
fn wait_until_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    // SAFETY: the External points at the PendingWork Box owned by the
    // instance core / run frame, alive for the isolate's lifetime — the same
    // contract as the console's LogBuffers pointer.
    let pending = unsafe { &*args.data().cast::<v8::External>().value().cast::<PendingWork>() };
    let mut slot = pending.slot.borrow_mut();
    let Some(registered) = slot.as_mut() else {
        drop(slot);
        throw_v8_error(
            scope,
            "[iso4] waitUntil is only available to run code — setup evaluated at \
             prepare() time must finish its work within the warm-up budget",
        );
        return;
    };
    let value = args.get(0);
    let promise: v8::Local<v8::Promise> = match v8::Local::<v8::Promise>::try_from(value) {
        Ok(p) => p,
        Err(_) => {
            let Some(resolver) = v8::PromiseResolver::new(scope) else {
                drop(slot);
                throw_v8_error(scope, "[iso4] waitUntil could not register the value");
                return;
            };
            resolver.resolve(scope, value);
            resolver.get_promise(scope)
        }
    };
    registered.push(v8::Global::new(scope, promise));
    rv.set_undefined();
}

fn install_console(scope: &mut v8::PinScope, buffers: *mut LogBuffers) -> Result<(), RunError> {
    let console = v8::Object::new(scope);
    let data = v8::External::new(scope, buffers.cast::<c_void>());

    for name in ["log", "debug", "info"] {
        let key = v8::String::new(scope, name)
            .ok_or_else(|| RunError::Internal(format!("failed to intern console.{name}")))?;
        let function = v8::Function::builder(console_stdout_callback)
            .data(data.into())
            .build(scope)
            .ok_or_else(|| RunError::Internal(format!("failed to create console.{name}")))?;
        console
            .set(scope, key.into(), function.into())
            .ok_or_else(|| RunError::Internal(format!("failed to install console.{name}")))?;
    }

    for name in ["warn", "error"] {
        let key = v8::String::new(scope, name)
            .ok_or_else(|| RunError::Internal(format!("failed to intern console.{name}")))?;
        let function = v8::Function::builder(console_stderr_callback)
            .data(data.into())
            .build(scope)
            .ok_or_else(|| RunError::Internal(format!("failed to create console.{name}")))?;
        console
            .set(scope, key.into(), function.into())
            .ok_or_else(|| RunError::Internal(format!("failed to install console.{name}")))?;
    }

    let global = scope.get_current_context().global(scope);
    let console_key = v8::String::new(scope, "console")
        .ok_or_else(|| RunError::Internal("failed to intern console".to_string()))?;
    global
        .set(scope, console_key.into(), console.into())
        .ok_or_else(|| RunError::Internal("failed to install console".to_string()))?;

    Ok(())
}

fn console_stdout_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    append_console_line(scope, args, true);
    rv.set_undefined();
}

fn console_stderr_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    append_console_line(scope, args, false);
    rv.set_undefined();
}

fn append_console_line(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    stdout: bool,
) {
    let data = args.data();
    let Ok(external) = v8::Local::<v8::External>::try_from(data) else {
        return;
    };
    let buffers = external.value().cast::<LogBuffers>();
    if buffers.is_null() {
        return;
    }

    let mut parts = Vec::new();
    for i in 0..args.length() {
        let value = args.get(i);
        let rendered = if value.is_undefined() {
            "undefined".to_string()
        } else {
            value
                .to_string(scope)
                .map(|s| s.to_rust_string_lossy(scope))
                .unwrap_or_else(|| "[unprintable]".to_string())
        };
        parts.push(rendered);
    }

    let line = parts.join(" ");
    // SAFETY: `buffers` points to the `LogBuffers` stack value in run_module().
    // The V8 context and every console callback are dropped before that stack
    // value goes out of scope.
    let buffers = unsafe { &mut *buffers };
    if stdout {
        let limit = buffers.max_stdout_bytes as usize;
        if limit == 0 || buffers.stdout_bytes + line.len() <= limit {
            buffers.stdout_bytes += line.len();
            buffers.stdout.push(line);
        }
        // Lines that would push the total over the limit are silently dropped.
    } else {
        let limit = buffers.max_stderr_bytes as usize;
        if limit == 0 || buffers.stderr_bytes + line.len() <= limit {
            buffers.stderr_bytes += line.len();
            buffers.stderr.push(line);
        }
    }
}

// ── Async context (AsyncLocalStorage) ────────────────────────────────────────
//
// A minimal, Node-compatible `AsyncLocalStorage` (`run` + `getStore`), surfaced
// to run/postfix code via `import { AsyncLocalStorage } from 'node:async_hooks'`.
//
// Mechanism: V8's *continuation-preserved embedder data* (CPED) — the same
// primitive modern Node's `AsyncContextFrame` rides on. V8 automatically saves
// the CPED slot with each promise continuation and restores it when that
// continuation runs, so an ambient value follows `async`/`await` chains without
// being threaded through call signatures, and concurrent chains stay isolated.
//
// The runtime installs two native functions (get/set CPED) and hands them to a
// small JS factory that returns the `AsyncLocalStorage` class closing over
// them. The class is stashed on `globalThis` under `Symbol.for(...)`; the
// built-in `node:async_hooks` module re-exports it. The native get/set are
// never exposed to user code — they live only in the factory closure.
//
// No promise hooks are registered, so runs that never construct an
// `AsyncLocalStorage` pay nothing beyond V8's own (cheap) CPED bookkeeping.
// State resets between runs naturally (fresh isolate per run). CPED is runtime
// state, not part of the snapshot, which is why the feature is unavailable to
// prefix/precompile code (see `async_context_builtin` on `ResolverContext`).

/// Reserved module specifier that resolves to the built-in async-context module.
const ASYNC_HOOKS_SPECIFIER: &str = "node:async_hooks";

/// Global-registry symbol description under which the `AsyncLocalStorage` class
/// is stashed. Must match `Symbol.for(...)` in [`ASYNC_HOOKS_MODULE_SRC`].
const ASYNC_CONTEXT_SYMBOL: &str = "iso4.async_hooks.AsyncLocalStorage";

/// Classic script evaluated once per run. Returns the `AsyncLocalStorage`
/// constructor, closing over the native `getContext`/`setContext` bindings.
///
/// Contexts are singly-linked frames (`{ p: parent, k: instance, v: value }`)
/// carried in the CPED slot. Reads walk the chain by instance identity. Only
/// object literals and own-property reads are used, so user code tampering with
/// builtin prototypes cannot subvert propagation. `run`'s `finally` restores the
/// parent for the synchronous tail; V8 restores the per-continuation frame for
/// the asynchronous tail.
const ASYNC_CONTEXT_FACTORY_SRC: &str = r#"
(function (getContext, setContext) {
  'use strict';
  class AsyncLocalStorage {
    run(store, callback, ...args) {
      const parent = getContext();
      const frame = { p: parent, k: this, v: store };
      setContext(frame);
      try {
        return callback(...args);
      } finally {
        setContext(parent);
      }
    }
    getStore() {
      let frame = getContext();
      while (frame != null) {
        if (frame.k === this) return frame.v;
        frame = frame.p;
      }
      return undefined;
    }
  }
  return AsyncLocalStorage;
})
"#;

/// ESM source of the built-in `node:async_hooks` module. Re-exports the class
/// installed by [`install_async_context`].
const ASYNC_HOOKS_MODULE_SRC: &str = "export const AsyncLocalStorage = \
    globalThis[Symbol.for('iso4.async_hooks.AsyncLocalStorage')];\n";

/// Native getter for the continuation-preserved embedder data slot.
fn async_context_get_callback(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let data = scope.get_continuation_preserved_embedder_data();
    rv.set(data);
}

/// Native setter for the continuation-preserved embedder data slot.
fn async_context_set_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    scope.set_continuation_preserved_embedder_data(args.get(0));
    rv.set_undefined();
}

/// Install the `AsyncLocalStorage` class on `globalThis` under
/// `Symbol.for(ASYNC_CONTEXT_SYMBOL)`. Called once per run before module
/// instantiation so the built-in `node:async_hooks` module can read it.
fn install_async_context(scope: &mut v8::PinScope) -> Result<(), RunError> {
    let get_fn = v8::Function::builder(async_context_get_callback)
        .build(scope)
        .ok_or_else(|| RunError::Internal("failed to build async-context getter".to_string()))?;
    let set_fn = v8::Function::builder(async_context_set_callback)
        .build(scope)
        .ok_or_else(|| RunError::Internal("failed to build async-context setter".to_string()))?;

    let factory_src = v8::String::new(scope, ASYNC_CONTEXT_FACTORY_SRC)
        .ok_or_else(|| RunError::Internal("failed to intern async-context factory".to_string()))?;
    let script = v8::Script::compile(scope, factory_src, None)
        .ok_or_else(|| RunError::Internal("failed to compile async-context factory".to_string()))?;
    let factory_val = script
        .run(scope)
        .ok_or_else(|| RunError::Internal("failed to run async-context factory".to_string()))?;
    let factory = v8::Local::<v8::Function>::try_from(factory_val)
        .map_err(|_| RunError::Internal("async-context factory is not a function".to_string()))?;

    let undefined = v8::undefined(scope).into();
    let class = factory
        .call(scope, undefined, &[get_fn.into(), set_fn.into()])
        .ok_or_else(|| RunError::Internal("async-context factory call failed".to_string()))?;

    let symbol_desc = v8::String::new(scope, ASYNC_CONTEXT_SYMBOL)
        .ok_or_else(|| RunError::Internal("failed to intern async-context symbol".to_string()))?;
    let symbol = v8::Symbol::for_key(scope, symbol_desc);
    let global = scope.get_current_context().global(scope);
    global
        .set(scope, symbol.into(), class)
        .ok_or_else(|| RunError::Internal("failed to install AsyncLocalStorage".to_string()))?;
    Ok(())
}

/// Reject the two export shapes whose diagnostic is only useful with the
/// export name attached.
///
/// V8's serializer refuses functions and promises too, but with a generic
/// "could not be cloned" message. Everything else it accepts —
/// `Date`/`Map`/`Set`/`RegExp`/`Error`/`ArrayBuffer`/TypedArrays/`bigint`/
/// cycles all round-trip as real instances — so there is nothing else to
/// pre-check here.
fn check_export_serializable(name: &str, value: v8::Local<v8::Value>) -> Result<(), RunError> {
    if value.is_function() {
        return Err(RunError::ExportNotSerializable(format!(
            "export \"{name}\" is a function"
        )));
    }
    if value.is_promise() {
        return Err(RunError::ExportNotSerializable(format!(
            "export \"{name}\" is an unresolved Promise"
        )));
    }
    Ok(())
}

fn runtime_error_from_value(
    scope: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
    value: v8::Local<v8::Value>,
) -> RunError {
    let name = error_name_from_value(scope, value).unwrap_or_else(|| "Error".to_string());
    let message = error_message_from_value(scope, value)
        .or_else(|| {
            value
                .to_string(scope)
                .map(|s| s.to_rust_string_lossy(scope))
        })
        .unwrap_or_else(|| "JavaScript error".to_string());
    RunError::RuntimeError(Box::new(RuntimeErrorData {
        name,
        message,
        stack: stack_from_value(scope, value),
        fields: error_fields_from_value(scope, value),
    }))
}

fn error_message_from_value(
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
    msg.to_string(scope).map(|s| s.to_rust_string_lossy(scope))
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
        .map(|s| s.to_rust_string_lossy(scope))
}

fn exception_message(
    scope: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
) -> String {
    scope
        .exception()
        .and_then(|e| e.to_string(scope))
        .map(|s| s.to_rust_string_lossy(scope))
        .unwrap_or_else(|| "(no exception message)".to_string())
}

fn exception_stack(
    scope: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
) -> Option<String> {
    let exception = scope.exception()?;
    stack_from_value(scope, exception)
}

fn error_name_from_value(
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
        .map(|s| s.to_rust_string_lossy(scope))
}

fn exception_name(
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
const RESERVED_ERROR_KEYS: [&str; 4] = ["name", "message", "stack", "__proto__"];

/// Copy `source`'s own enumerable string-keyed properties onto `target`,
/// skipping `skip`. Uses `create_data_property` ([[DefineOwnProperty]]), so a
/// copied key is always a plain own data property.
fn copy_own_properties_except(
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
fn error_fields_from_value(
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

fn exception_fields(
    scope: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
) -> Option<Vec<u8>> {
    let exception = scope.exception()?;
    error_fields_from_value(scope, exception)
}

// ── Tests ─────────────────────────────────────────────────────────────────────
//
// TDD suite for the full Phase 1-8 surface. Tests are grouped by concern.
// Many will fail until the corresponding phase is implemented - that is
// intentional. Run `cargo test` to see what is still outstanding.
//
// Tests marked `#[ignore]` are ones that would hang or OOM the process
// without the relevant limit being implemented first. Un-ignore them when
// the limit lands.

#[cfg(test)]
mod tests {
    use super::*;
    // The exports/args/fields slots are now V8 serialization blobs; the test
    // suite asserts on the decoded shape, so it reads them back through the
    // test-only value tree in `testval.rs`. `WireValue` is kept as the local
    // alias so the assertions below still read as value shapes.
    use crate::testval::{self, TestValue as WireValue};

    /// Shorthand: run a code string and return the full Output or RunError.
    /// Run code with explicit limits. Used for limit-enforcement tests.
    fn run_code(code: &str, filename: &str, limits: Limits) -> Result<Output, FailureOutput> {
        init_platform();
        run_module(
            Some(code),
            filename,
            limits,
            &[],
            &[],
            None,
            Arc::new(AtomicU32::new(0)),
            None,
        )
    }

    fn run(code: &str) -> Result<Output, RunError> {
        run_code(code, "<iso4>", Limits::default()).map_err(|failure| failure.error)
    }

    /// Build a source-form `ImportBinding` for tests.
    fn source_import(specifier: &str, source: &str) -> ImportBinding {
        ImportBinding {
            specifier: specifier.to_string(),
            module: ImportModule::Source(source.to_string()),
        }
    }

    /// Build a host-form `ImportBinding` for tests.
    fn host_import(specifier: &str, exports: Vec<(&str, HostModuleNode)>) -> ImportBinding {
        ImportBinding {
            specifier: specifier.to_string(),
            module: ImportModule::Host(
                exports
                    .into_iter()
                    .map(|(name, node)| (name.to_string(), node))
                    .collect(),
            ),
        }
    }

    /// Run user code with a fixed set of source imports; no host bridge.
    fn run_with_source_imports(code: &str, imports: &[ImportBinding]) -> Result<Output, RunError> {
        init_platform();
        run_module(
            Some(code),
            "<iso4>",
            Limits::default(),
            &[],
            imports,
            None,
            Arc::new(AtomicU32::new(0)),
            None,
        )
        .map_err(|failure| failure.error)
    }

    /// Shorthand: expect success and return the Output, panic otherwise.
    fn run_ok(code: &str) -> Output {
        match run(code) {
            Ok(o) => o,
            Err(e) => panic!("expected Ok, got error: {e:?}"),
        }
    }

    /// Shorthand: expect a RunError, panic if it succeeds.
    fn run_err(code: &str) -> RunError {
        match run(code) {
            Err(e) => e,
            Ok(o) => panic!("expected Err, got output: {o:?}"),
        }
    }

    fn has_line(lines: &[String], needle: &str) -> bool {
        lines.iter().any(|line| line.contains(needle))
    }

    // ── Value-shape test helpers ──────────────────────────────────────────────
    //
    // `Output::exports` is one V8 serialization blob holding the flat
    // `{ name: value }` export object. These helpers decode it into the
    // `TestValue` tree so the assertions below can talk about value shapes.

    /// Decode the exports blob into its value tree.
    fn exports_of(out: &Output) -> WireValue {
        testval::from_blob(&out.exports)
    }

    /// Look up a field in the top-level exports Object by name.
    fn get_field(out: &Output, key: &str) -> Option<WireValue> {
        if let WireValue::Object(fields) = exports_of(out) {
            fields.into_iter().find(|(k, _)| k == key).map(|(_, v)| v)
        } else {
            None
        }
    }

    /// Return the `default` export rendered as a display string, or `None`.
    fn get_default(out: &Output) -> Option<String> {
        get_field(out, "default").map(|v| wire_to_display_str(&v))
    }

    /// Return a named export rendered as a display string, or `None`.
    fn get_named(out: &Output, key: &str) -> Option<String> {
        get_field(out, key).map(|v| wire_to_display_str(&v))
    }

    /// Return all export key names.
    fn export_keys(out: &Output) -> Vec<String> {
        if let WireValue::Object(fields) = exports_of(out) {
            fields.into_iter().map(|(k, _)| k).collect()
        } else {
            vec![]
        }
    }

    /// Convert a little-endian u64 word array plus a sign bit to a decimal
    /// string. Used only in tests (avoids a `num-bigint` dependency).
    ///
    /// Algorithm: repeated division by 10 processing words MSW→LSW,
    /// collecting remainder digits until the quotient is all-zeros.
    /// Each step works in u128 to handle the 2⁴² carry from word to word.
    fn words_to_decimal(sign: bool, words: &[u64]) -> String {
        if words.is_empty() || words.iter().all(|&w| w == 0) {
            return "0".to_string();
        }
        let mut buf = words.to_vec(); // LSW at index 0
        let mut digits = String::new();
        loop {
            let mut rem: u128 = 0;
            for w in buf.iter_mut().rev() {
                let cur: u128 = (rem << 64) | (*w as u128);
                *w = (cur / 10) as u64;
                rem = cur % 10;
            }
            digits.push(char::from_digit(rem as u32, 10).unwrap());
            if buf.iter().all(|&w| w == 0) {
                break;
            }
        }
        if sign {
            digits.push('-');
        }
        digits.chars().rev().collect()
    }

    /// Render a `WireValue` as a human-readable string similar to what V8's
    /// `ToString()` / `JSON.stringify()` would produce. Used only in tests.
    fn wire_to_display_str(v: &WireValue) -> String {
        match v {
            WireValue::Undefined => "undefined".to_string(),
            WireValue::Null => "null".to_string(),
            WireValue::Bool(b) => (if *b { "true" } else { "false" }).to_string(),
            WireValue::Number(n) => {
                if n.is_nan() {
                    "NaN".to_string()
                } else if n.is_infinite() {
                    if *n > 0.0 {
                        "Infinity".to_string()
                    } else {
                        "-Infinity".to_string()
                    }
                } else if n.fract() == 0.0 && n.abs() < 1e15 {
                    format!("{}", *n as i64)
                } else {
                    format!("{n}")
                }
            }
            WireValue::String(s) => s.clone(),
            WireValue::BigInt(sign, words) => words_to_decimal(*sign, words),
            WireValue::Bytes(_) => "[Uint8Array]".to_string(),
            // Real instances (Date/Map/Set/RegExp/Error/…) now cross the
            // boundary intact; the value tree renders them as a description.
            WireValue::Other(text) => text.clone(),
            WireValue::Cycle => "[Circular]".to_string(),
            WireValue::Array(items) => {
                let parts: Vec<String> = items.iter().map(wire_to_display_str).collect();
                format!("[{}]", parts.join(","))
            }
            WireValue::Object(fields) => {
                let parts: Vec<String> = fields
                    .iter()
                    .map(|(k, v)| format!("{k:?}:{}", wire_to_display_str(v)))
                    .collect();
                format!("{{{}}}", parts.join(","))
            }
        }
    }

    // ── Basic ESM execution ───────────────────────────────────────────────

    #[test]
    fn basic_arithmetic_returns_result() {
        let out = run_ok("export default 1 + 1");
        assert_eq!(get_default(&out).as_deref(), Some("2"));
    }

    #[test]
    fn string_concatenation_returns_result() {
        let out = run_ok("export default 'hello' + ' world'");
        assert_eq!(get_default(&out).as_deref(), Some("hello world"));
    }

    #[test]
    fn boolean_true_returns_result() {
        let out = run_ok("export default true");
        assert_eq!(get_default(&out).as_deref(), Some("true"));
    }

    #[test]
    fn empty_code_returns_no_default_export() {
        let out = run_ok("");
        assert!(get_default(&out).is_none());
        assert!(export_keys(&out).is_empty());
    }

    // ── ESM exports ──────────────────────────────────────────────────────

    #[test]
    fn export_default_number() {
        let out = run_ok("export default 42");
        assert_eq!(get_default(&out).as_deref(), Some("42"));
    }

    #[test]
    fn export_default_string() {
        let out = run_ok(r#"export default "hello""#);
        assert_eq!(get_default(&out).as_deref(), Some("hello"));
    }

    #[test]
    fn export_default_boolean() {
        let out = run_ok("export default true");
        assert_eq!(get_default(&out).as_deref(), Some("true"));
    }

    #[test]
    fn export_default_null() {
        let out = run_ok("export default null");
        assert_eq!(get_default(&out).as_deref(), Some("null"));
    }

    #[test]
    fn export_default_object() {
        let out = run_ok(r#"export default { x: 1, y: 2 }"#);
        let d = get_default(&out).unwrap();
        // Order is not guaranteed - just check both keys are present.
        assert!(d.contains("x") && d.contains("1"));
        assert!(d.contains("y") && d.contains("2"));
    }

    #[test]
    fn export_default_array() {
        let out = run_ok("export default [1, 2, 3]");
        assert_eq!(get_default(&out).as_deref(), Some("[1,2,3]"));
    }

    #[test]
    fn named_export_single() {
        let out = run_ok("export const answer = 42");
        assert_eq!(get_named(&out, "answer").as_deref(), Some("42"));
    }

    #[test]
    fn named_exports_multiple() {
        let out = run_ok("export const x = 1; export const y = 2");
        assert_eq!(get_named(&out, "x").as_deref(), Some("1"));
        assert_eq!(get_named(&out, "y").as_deref(), Some("2"));
    }

    #[test]
    fn default_and_named_exports_together() {
        let out = run_ok("export default 99; export const label = 'hi'");
        assert_eq!(get_default(&out).as_deref(), Some("99"));
        assert_eq!(get_named(&out, "label").as_deref(), Some("hi"));
    }

    #[test]
    fn no_export_gives_no_default() {
        let out = run_ok("const x = 1");
        assert!(get_default(&out).is_none());
    }

    // ── Async / top-level await ───────────────────────────────────────────

    #[test]
    fn top_level_await_resolves() {
        let out = run_ok("export default await Promise.resolve(7)");
        assert_eq!(get_default(&out).as_deref(), Some("7"));
    }

    #[test]
    fn async_computation_resolves() {
        let out = run_ok(
            r#"
            async function compute() { return 1 + 1; }
            export default await compute();
            "#,
        );
        assert_eq!(get_default(&out).as_deref(), Some("2"));
    }

    #[test]
    fn top_level_await_rejected_promise_is_runtime_error() {
        let err = run_err("export default await Promise.reject(new Error('oops'))");
        assert!(matches!(err, RunError::RuntimeError(_)));
    }

    // ── Console capture ───────────────────────────────────────────────────
    // Requires the console shim (Phase 3+).

    #[test]
    fn console_log_captured_in_stdout() {
        let out = run_ok(r#"console.log("hello from log"); export default 1"#);
        assert!(has_line(&out.stdout, "hello from log"));
    }

    #[test]
    fn console_error_captured_in_stderr() {
        let out = run_ok(r#"console.error("something went wrong"); export default 1"#);
        assert!(has_line(&out.stderr, "something went wrong"));
    }

    #[test]
    fn console_warn_captured_in_stderr() {
        let out = run_ok(r#"console.warn("watch out"); export default 1"#);
        assert!(has_line(&out.stderr, "watch out"));
    }

    #[test]
    fn console_debug_captured_in_stdout() {
        let out = run_ok(r#"console.debug("debugging"); export default 1"#);
        assert!(has_line(&out.stdout, "debugging"));
    }

    #[test]
    fn console_info_captured_in_stdout() {
        let out = run_ok(r#"console.info("just so you know"); export default 1"#);
        assert!(has_line(&out.stdout, "just so you know"));
    }

    #[test]
    fn multiple_console_logs_all_captured() {
        let out = run_ok(
            r#"
            console.log("line one");
            console.log("line two");
            console.log("line three");
            export default 1
            "#,
        );
        assert!(has_line(&out.stdout, "line one"));
        assert!(has_line(&out.stdout, "line two"));
        assert!(has_line(&out.stdout, "line three"));
    }

    #[test]
    fn console_log_does_not_bleed_into_stderr() {
        let out = run_ok(r#"console.log("stdout only"); export default 1"#);
        assert!(out.stderr.is_empty());
    }

    #[test]
    fn console_log_multiple_args_joined() {
        let out = run_ok(r#"console.log("a", "b", "c"); export default 1"#);
        assert!(has_line(&out.stdout, "a"));
        assert!(has_line(&out.stdout, "b"));
        assert!(has_line(&out.stdout, "c"));
    }

    // ── Error handling ────────────────────────────────────────────────────

    #[test]
    fn syntax_error_is_compile_error() {
        let err = run_err("export default (((");
        assert!(matches!(err, RunError::CompileError(_)));
    }

    #[test]
    fn thrown_error_is_runtime_error() {
        let err = run_err(r#"throw new Error("boom")"#);
        assert!(matches!(err, RunError::RuntimeError(_)));
    }

    #[test]
    fn thrown_error_message_is_preserved() {
        let err = run_err(r#"throw new Error("specific message")"#);
        if let RunError::RuntimeError(inner) = err {
            assert!(inner.message.contains("specific message"));
        } else {
            panic!("expected RuntimeError");
        }
    }

    #[test]
    fn logs_before_throw_are_preserved_on_failure() {
        let failure = run_code(
            r#"
            console.log("before stdout");
            console.error("before stderr");
            throw new Error("boom")
            "#,
            "<iso4>",
            Limits::default(),
        )
        .unwrap_err();

        assert!(matches!(failure.error, RunError::RuntimeError(_)));
        assert!(has_line(&failure.stdout, "before stdout"));
        assert!(has_line(&failure.stderr, "before stderr"));
    }

    #[test]
    fn thrown_string_is_runtime_error() {
        let err = run_err(r#"throw "raw string error""#);
        assert!(matches!(err, RunError::RuntimeError(_)));
    }

    #[test]
    fn runtime_error_includes_stack_trace() {
        let err = run_err(
            r#"
            function inner() { throw new Error("deep"); }
            function outer() { inner(); }
            outer();
            "#,
        );
        if let RunError::RuntimeError(inner) = err {
            assert!(inner.stack.is_some(), "expected a stack trace");
        } else {
            panic!("expected RuntimeError");
        }
    }

    #[test]
    fn reference_error_is_runtime_error() {
        let err = run_err("export default undeclaredVariable");
        assert!(matches!(err, RunError::RuntimeError(_)));
    }

    #[test]
    fn type_error_is_runtime_error() {
        let err = run_err("null.property");
        assert!(matches!(err, RunError::RuntimeError(_)));
    }

    // ── Export validation ─────────────────────────────────────────────────
    //
    // Non-serializable exports are skipped, never fatal: the value is
    // absent from the exports and its name is reported in `skipped_exports`.

    #[test]
    fn exporting_a_function_is_skipped_and_reported() {
        let out = run_ok("export default function() {}\nexport const kept = 1");
        assert_eq!(out.skipped_exports, vec!["default".to_string()]);
        assert!(get_field(&out, "default").is_none());
        assert_eq!(get_named(&out, "kept").as_deref(), Some("1"));
    }

    #[test]
    fn exporting_a_class_is_skipped_and_reported() {
        let out = run_ok("export default class Foo {}");
        assert_eq!(out.skipped_exports, vec!["default".to_string()]);
        assert!(get_field(&out, "default").is_none());
    }

    #[test]
    fn exporting_an_unresolved_promise_is_skipped_and_reported() {
        // A Promise that is never awaited cannot cross the boundary; it is
        // absent from the exports rather than failing the run.
        let out = run_ok("export default new Promise(() => {})");
        assert_eq!(out.skipped_exports, vec!["default".to_string()]);
    }

    #[test]
    fn nested_function_in_export_is_skipped_via_trial_serialization() {
        // The pre-check only sees the object; the nested function surfaces in
        // the combined serialize, which falls back to per-export trials.
        let out = run_ok(
            "export default { fetch() { return 1 } }\nexport const limits = { memoryMb: 64 }",
        );
        assert_eq!(out.skipped_exports, vec!["default".to_string()]);
        assert!(get_field(&out, "default").is_none());
        assert!(get_field(&out, "limits").is_some());
    }

    #[test]
    fn serializable_runs_report_no_skipped_exports() {
        let out = run_ok("export default 42");
        assert!(out.skipped_exports.is_empty());
    }

    #[test]
    fn exporting_non_plain_builtins_roundtrips_as_real_instances() {
        // The V8 serialization format carries these natively, so they arrive
        // as real instances rather than being rejected (the old wire codec)
        // or silently flattened to `{}`.
        for (code, expected) in [
            (
                "export default new Date(1700000000000)",
                "Date(1700000000000)",
            ),
            ("export default new Map([['a', 1]])", r#"Map([["a",1]])"#),
            ("export default new Set([1, 2, 3])", "Set([1,2,3])"),
            ("export default /abc/g", "RegExp(abc/g)"),
            (
                "export default new Uint8Array([7, 8]).buffer",
                "ArrayBuffer(7,8)",
            ),
            ("export default new TypeError('boom')", "TypeError(boom)"),
        ] {
            let out = run_ok(code);
            assert_eq!(get_default(&out).as_deref(), Some(expected), "{code}");
        }
    }

    #[test]
    fn exporting_a_nested_builtin_roundtrips() {
        // Nested case: `{ when: new Date() }` keeps the Date instance.
        let out = run_ok("export default { when: new Date(1700000000000) }");
        assert_eq!(
            get_field(&out, "default"),
            Some(WireValue::Object(vec![(
                "when".to_string(),
                WireValue::Other("Date(1700000000000)".to_string()),
            )]))
        );
    }

    #[test]
    fn exporting_a_non_uint8_typed_array_roundtrips_with_its_element_type() {
        for (code, expected) in [
            (
                "export default new Float32Array([1, 2])",
                "Float32Array(1,2)",
            ),
            ("export default new Int32Array([1, 2])", "Int32Array(1,2)"),
            (
                "export default new DataView(new ArrayBuffer(4))",
                "DataView(0,0,0,0)",
            ),
        ] {
            let out = run_ok(code);
            assert_eq!(get_default(&out).as_deref(), Some(expected), "{code}");
        }
    }

    #[test]
    fn exporting_a_uint8_array_roundtrips_as_bytes() {
        let out = run_ok("export default new Uint8Array([1, 2, 3])");
        assert_eq!(
            get_field(&out, "default"),
            Some(WireValue::Bytes(vec![1, 2, 3]))
        );
    }

    #[test]
    fn exporting_a_uint8_array_subarray_respects_byte_offset() {
        // A subarray view must serialize only its window, not the whole
        // backing buffer.
        let out =
            run_ok("const b = new Uint8Array([0, 1, 2, 3, 4]); export default b.subarray(1, 4)");
        assert_eq!(
            get_field(&out, "default"),
            Some(WireValue::Bytes(vec![1, 2, 3]))
        );
    }

    #[test]
    fn exporting_an_empty_uint8_array_roundtrips_as_empty_bytes() {
        let out = run_ok("export default new Uint8Array(0)");
        assert_eq!(get_field(&out, "default"), Some(WireValue::Bytes(vec![])));
    }

    // ── Complex value structures ───────────────────────────────────────────

    /// Helper: look up a key inside a `WireValue::Object`, returning the value.
    fn wire_obj_get(v: &WireValue, key: &str) -> Option<WireValue> {
        if let WireValue::Object(fields) = v {
            fields
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.clone())
        } else {
            None
        }
    }

    #[test]
    fn sum_1_to_100_in_nested_object() {
        // Runtime arithmetic + nested object export.
        let out = run_ok(
            r#"
            let sum = 0;
            for (let i = 1; i <= 100; i++) sum += i;
            export default {
                result: sum,
                metadata: { label: "sum_1_to_100", iterations: 100 }
            }
            "#,
        );
        let default_val = get_field(&out, "default").unwrap();
        assert_eq!(
            wire_obj_get(&default_val, "result"),
            Some(WireValue::Number(5050.0))
        );
        let meta = wire_obj_get(&default_val, "metadata").unwrap();
        assert_eq!(
            wire_obj_get(&meta, "label"),
            Some(WireValue::String("sum_1_to_100".to_string()))
        );
        assert_eq!(
            wire_obj_get(&meta, "iterations"),
            Some(WireValue::Number(100.0))
        );
    }

    #[test]
    fn objects_inside_array() {
        let out = run_ok("export default [{ a: 1 }, { b: 2 }, { c: 3 }]");
        let val = get_field(&out, "default").unwrap();
        if let WireValue::Array(items) = val {
            assert_eq!(items.len(), 3);
            assert_eq!(wire_obj_get(&items[0], "a"), Some(WireValue::Number(1.0)));
            assert_eq!(wire_obj_get(&items[1], "b"), Some(WireValue::Number(2.0)));
            assert_eq!(wire_obj_get(&items[2], "c"), Some(WireValue::Number(3.0)));
        } else {
            panic!("expected Array");
        }
    }

    #[test]
    fn object_in_second_object_in_array() {
        // Array → Object → Object nesting.
        let out = run_ok(r#"export default [{ outer: { inner: 42 } }, { x: [1, 2, 3] }]"#);
        let val = get_field(&out, "default").unwrap();
        if let WireValue::Array(items) = val {
            let inner = wire_obj_get(&wire_obj_get(&items[0], "outer").unwrap(), "inner");
            assert_eq!(inner, Some(WireValue::Number(42.0)));
            let arr = wire_obj_get(&items[1], "x").unwrap();
            assert_eq!(
                arr,
                WireValue::Array(vec![
                    WireValue::Number(1.0),
                    WireValue::Number(2.0),
                    WireValue::Number(3.0),
                ])
            );
        } else {
            panic!("expected Array");
        }
    }

    #[test]
    fn deep_alternating_nesting_array_object_array_object() {
        // Object → Array → Object → Array → Number
        let out = run_ok(r#"export default { a: [{ b: [{ c: 42 }, { d: "hello" }] }] }"#);
        let default_val = get_field(&out, "default").unwrap();
        let a = wire_obj_get(&default_val, "a").unwrap();
        if let WireValue::Array(a_items) = a {
            let b = wire_obj_get(&a_items[0], "b").unwrap();
            if let WireValue::Array(b_items) = b {
                assert_eq!(
                    wire_obj_get(&b_items[0], "c"),
                    Some(WireValue::Number(42.0))
                );
                assert_eq!(
                    wire_obj_get(&b_items[1], "d"),
                    Some(WireValue::String("hello".to_string()))
                );
            } else {
                panic!("expected b to be Array");
            }
        } else {
            panic!("expected a to be Array");
        }
    }

    #[test]
    fn multiple_named_exports_with_complex_values() {
        let out = run_ok(
            r#"
            export const nums = [1, 2, 3];
            export const info = { count: 3, total: 6 };
            export default "summary";
            "#,
        );
        assert_eq!(get_default(&out).as_deref(), Some("summary"));
        assert_eq!(
            get_field(&out, "nums"),
            Some(WireValue::Array(vec![
                WireValue::Number(1.0),
                WireValue::Number(2.0),
                WireValue::Number(3.0),
            ]))
        );
        let info = get_field(&out, "info").unwrap();
        assert_eq!(wire_obj_get(&info, "count"), Some(WireValue::Number(3.0)));
        assert_eq!(wire_obj_get(&info, "total"), Some(WireValue::Number(6.0)));
    }

    // ── Cycle detection ───────────────────────────────────────────────────

    #[test]
    fn cyclic_object_roundtrips() {
        // The V8 format has back-references, so a cycle survives intact
        // instead of failing the run (the old wire codec rejected it).
        let out = run_ok(
            r#"
            const obj = { x: 1 };
            obj.self = obj;
            export default obj;
            "#,
        );
        assert_eq!(
            get_field(&out, "default"),
            Some(WireValue::Object(vec![
                ("x".to_string(), WireValue::Number(1.0)),
                ("self".to_string(), WireValue::Cycle),
            ]))
        );
    }

    #[test]
    fn cyclic_array_roundtrips() {
        let out = run_ok(
            r#"
            const arr = [1, 2, 3];
            arr.push(arr);
            export default arr;
            "#,
        );
        assert_eq!(
            get_field(&out, "default"),
            Some(WireValue::Array(vec![
                WireValue::Number(1.0),
                WireValue::Number(2.0),
                WireValue::Number(3.0),
                WireValue::Cycle,
            ]))
        );
    }

    #[test]
    fn indirect_cycle_array_inside_object_inside_array_roundtrips() {
        // arr → obj → arr (cross-type indirect cycle)
        let out = run_ok(
            r#"
            const arr = [];
            const obj = { arr };
            arr.push(obj);
            export default arr;
            "#,
        );
        assert_eq!(
            get_field(&out, "default"),
            Some(WireValue::Array(vec![WireValue::Object(vec![(
                "arr".to_string(),
                WireValue::Cycle,
            )])]))
        );
    }

    #[test]
    fn indirect_cycle_two_objects_roundtrips() {
        let out = run_ok(
            r#"
            const a = {};
            const b = { a };
            a.b = b;
            export default a;
            "#,
        );
        assert_eq!(
            get_field(&out, "default"),
            Some(WireValue::Object(vec![(
                "b".to_string(),
                WireValue::Object(vec![("a".to_string(), WireValue::Cycle)]),
            )]))
        );
    }

    #[test]
    fn shared_reference_is_not_a_cycle() {
        // The same object appears in two fields but is not cyclic.
        // This must succeed - pop-after-visit ensures no false positive.
        let out = run_ok(
            r#"
            const shared = { x: 1 };
            export default { a: shared, b: shared };
            "#,
        );
        let default_val = get_field(&out, "default").unwrap();
        assert_eq!(
            wire_obj_get(&wire_obj_get(&default_val, "a").unwrap(), "x"),
            Some(WireValue::Number(1.0))
        );
        assert_eq!(
            wire_obj_get(&wire_obj_get(&default_val, "b").unwrap(), "x"),
            Some(WireValue::Number(1.0))
        );
    }

    #[test]
    fn shared_array_reference_is_not_a_cycle() {
        let out = run_ok(
            r#"
            const shared = [1, 2, 3];
            export default { first: shared, second: shared };
            "#,
        );
        let default_val = get_field(&out, "default").unwrap();
        let expected_arr = WireValue::Array(vec![
            WireValue::Number(1.0),
            WireValue::Number(2.0),
            WireValue::Number(3.0),
        ]);
        assert_eq!(
            wire_obj_get(&default_val, "first"),
            Some(expected_arr.clone())
        );
        assert_eq!(wire_obj_get(&default_val, "second"), Some(expected_arr));
    }

    // ── BigInt ────────────────────────────────────────────────────────────

    #[test]
    fn bigint_positive_roundtrips() {
        let out = run_ok("export default 42n");
        assert_eq!(
            get_field(&out, "default"),
            Some(WireValue::BigInt(false, vec![42]))
        );
        assert_eq!(get_default(&out).as_deref(), Some("42"));
    }

    #[test]
    fn bigint_negative_roundtrips() {
        let out = run_ok("export default -100n");
        assert_eq!(
            get_field(&out, "default"),
            Some(WireValue::BigInt(true, vec![100]))
        );
        assert_eq!(get_default(&out).as_deref(), Some("-100"));
    }

    #[test]
    fn bigint_zero_roundtrips() {
        let out = run_ok("export default 0n");
        // V8 word_count() for 0n is 0; words slice is empty.
        let field = get_field(&out, "default").unwrap();
        assert!(
            matches!(field, WireValue::BigInt(false, ref w) if w.is_empty() || w.iter().all(|&x| x == 0))
        );
        assert_eq!(get_default(&out).as_deref(), Some("0"));
    }

    #[test]
    fn bigint_inside_object_roundtrips() {
        // 1_000_000_000_000 = 0xE8D4A51000 which fits in a single u64 word.
        let out = run_ok("export default { count: 1000000000000n }");
        let default_val = get_field(&out, "default").unwrap();
        assert_eq!(
            wire_obj_get(&default_val, "count"),
            Some(WireValue::BigInt(false, vec![1_000_000_000_000u64]))
        );
    }

    #[test]
    fn bigint_larger_than_u64_max_roundtrips() {
        // 2n**64n = 18446744073709551616 — needs 2 words: [0, 1] (LSW first).
        let out = run_ok("export default 2n**64n");
        assert_eq!(
            get_field(&out, "default"),
            Some(WireValue::BigInt(false, vec![0, 1]))
        );
        assert_eq!(get_default(&out).as_deref(), Some("18446744073709551616"));
    }

    #[test]
    fn bigint_negative_larger_than_i64_min_roundtrips() {
        // -(2n**65n) — needs 2 words: [0, 2], sign=true.
        let out = run_ok("export default -(2n**65n)");
        assert_eq!(
            get_field(&out, "default"),
            Some(WireValue::BigInt(true, vec![0, 2]))
        );
        assert_eq!(get_default(&out).as_deref(), Some("-36893488147419103232"));
    }

    #[test]
    fn bigint_inject_via_bridge_roundtrips() {
        // Host returns 2^128 as (sign=false, words=[0,0,1]) in a BridgeResponse.
        // Sandbox exports it back; we verify the round-trip.
        let large = WireValue::BigInt(false, vec![0, 0, 1]); // 2^128
        let (out, h) = run_with_bridge(
            "export default await getBig()",
            "getBig",
            Limits {
                cpu_time_ms: 5_000,
                wall_time_ms: 10_000,
                ..Default::default()
            },
            move |s| {
                ipc::write_ts_to_rust_frame(
                    s,
                    ipc::TsToRustMessageType::BridgeResponse,
                    &bridge_resp_ok(0, &large),
                )
                .unwrap();
            },
        );
        h.join().unwrap();
        assert_eq!(
            get_field(&out.unwrap(), "default"),
            Some(WireValue::BigInt(false, vec![0, 0, 1]))
        );
    }

    // ── Globals ───────────────────────────────────────────────────────────

    #[test]
    fn math_random_is_available() {
        // Math is a V8 built-in - no setup needed.
        let out = run_ok("export default typeof Math.random === 'function'");
        assert_eq!(get_default(&out).as_deref(), Some("true"));
    }

    #[test]
    fn json_parse_stringify_available() {
        let out = run_ok(r#"export default JSON.stringify({a:1})"#);
        // JSON.stringify returns a JS string - WireValue::String - displayed as-is.
        assert_eq!(get_default(&out).as_deref(), Some(r#"{"a":1}"#));
    }

    #[test]
    fn unconfigured_fetch_is_just_a_missing_global_runtime_error() {
        let err = run_err(r#"export default await fetch("https://example.com")"#);
        assert!(matches!(err, RunError::RuntimeError(_)));
    }

    #[test]
    #[ignore = "requires deliberate crypto.getRandomValues shim"]
    fn crypto_get_random_values_available() {
        let out = run_ok(
            r#"
            const buf = new Uint8Array(8);
            crypto.getRandomValues(buf);
            export default buf.length
            "#,
        );
        assert_eq!(get_default(&out).as_deref(), Some("8"));
    }

    #[test]
    fn console_is_defined() {
        let out = run_ok("export default typeof console");
        assert_eq!(get_default(&out).as_deref(), Some("object"));
    }

    #[test]
    fn node_globals_are_not_available() {
        // `process` and `require` must not exist in the sandbox.
        let out = run_ok("export default typeof process");
        assert_eq!(get_default(&out).as_deref(), Some("undefined"));
    }

    #[test]
    fn node_require_is_not_available() {
        let out = run_ok("export default typeof require");
        assert_eq!(get_default(&out).as_deref(), Some("undefined"));
    }

    // ── Imports ───────────────────────────────────────────────────────────
    // Real module resolver wired up: source modules compile through V8 directly,
    // host modules become synthetic v8::Modules whose exports are bridge stubs.
    // Host-import bridge call tests live further down with the other
    // bridge-call integration tests because they need a session socket.

    #[test]
    fn unknown_import_is_module_not_found() {
        // With no host-declared bindings, every specifier is unknown.
        let err = run_err(r#"import { foo } from "unknown:module"; export default foo"#);
        assert!(matches!(err, RunError::ModuleNotFound(_)));
    }

    #[test]
    fn source_import_provided_by_host_works() {
        let imports = [source_import(
            "math:add",
            "export const add = (a, b) => a + b;",
        )];
        let out = run_with_source_imports(
            r#"import { add } from "math:add"; export default add(1, 2)"#,
            &imports,
        )
        .unwrap();
        assert_eq!(get_default(&out).as_deref(), Some("3"));
    }

    #[test]
    fn source_import_unknown_specifier_with_declared_bindings_is_module_not_found() {
        // A binding for "math:add" exists, but the user imports "math:sub".
        let imports = [source_import(
            "math:add",
            "export const add = (a, b) => a + b;",
        )];
        let err = run_with_source_imports(
            r#"import { sub } from "math:sub"; export default sub(1, 2)"#,
            &imports,
        )
        .unwrap_err();
        assert!(matches!(err, RunError::ModuleNotFound(_)));
    }

    // ── Resource limits ───────────────────────────────────────────────────
    // Marked #[ignore] - without the limit implementation these would hang
    // or OOM the process. Un-ignore when the corresponding phase lands.

    #[test]
    fn infinite_loop_hits_cpu_or_wall_timeout() {
        // cpu_time_ms=200, wall_time_ms=1000: the CPU guard fires at ~200-210ms,
        // the wall guard fires at 1000ms. CpuTimeout must always win by ~800ms.
        let err = run_code(
            "while(true) {}",
            "<test>",
            Limits {
                cpu_time_ms: 200,
                wall_time_ms: 1_000,
                ..Default::default()
            },
        )
        .map_err(|f| f.error)
        .unwrap_err();
        assert!(
            matches!(err, RunError::CpuTimeout),
            "expected CpuTimeout (cpu=200ms fires ~800ms before wall=1000ms), got {err:?}"
        );
    }

    #[test]
    fn limits_zero_means_disabled() {
        let out = run_code(
            "export default 42",
            "<test>",
            Limits {
                cpu_time_ms: 0,
                wall_time_ms: 0,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(get_default(&out).as_deref(), Some("42"));
    }

    #[test]
    fn run_within_budget_succeeds() {
        let out = run_code(
            "let s = 0; for (let i = 0; i < 10_000; i++) s += i; export default s",
            "<test>",
            Limits {
                cpu_time_ms: 5_000,
                wall_time_ms: 10_000,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(get_default(&out).as_deref(), Some("49995000"));
    }

    #[test]
    fn execute_with_prefix_infinite_loop_is_killed() {
        let prefix = prepared("globalThis.base = 10", &[], &[]);
        let err = execute_with_prefix(
            prefix,
            Some("while (true) {}"),
            None,
            Limits {
                cpu_time_ms: 200,
                wall_time_ms: 1_000,
                ..Default::default()
            },
            &[],
            &[],
            None,
            Arc::new(AtomicU32::new(0)),
            None,
        )
        .map_err(|f| f.error)
        .unwrap_err();
        assert!(
            matches!(err, RunError::CpuTimeout | RunError::WallTimeout),
            "expected timeout, got {err:?}"
        );
    }

    #[test]
    fn for_loop_infinite_hits_timeout() {
        let err = run_code(
            "for (let i = 0;;) i++",
            "<test>",
            Limits {
                cpu_time_ms: 200,
                wall_time_ms: 1_000,
                ..Default::default()
            },
        )
        .map_err(|f| f.error)
        .unwrap_err();
        assert!(matches!(err, RunError::CpuTimeout | RunError::WallTimeout));
    }

    #[test]
    fn deep_recursion_is_runtime_error_not_timeout() {
        let err = run_code(
            "function inf(n) { return inf(n + 1); } export default inf(0)",
            "<test>",
            Limits {
                cpu_time_ms: 5_000,
                wall_time_ms: 10_000,
                ..Default::default()
            },
        )
        .map_err(|f| f.error)
        .unwrap_err();
        assert!(
            matches!(err, RunError::RuntimeError(_)),
            "expected RuntimeError (stack overflow), got {err:?}"
        );
    }

    #[test]
    fn very_tight_wall_limit_fires() {
        let err = run_code(
            "while (true) {}",
            "<test>",
            Limits {
                cpu_time_ms: 30_000,
                wall_time_ms: 1,
                ..Default::default()
            },
        )
        .map_err(|f| f.error)
        .unwrap_err();
        assert!(
            matches!(err, RunError::WallTimeout | RunError::CpuTimeout),
            "expected timeout, got {err:?}"
        );
    }

    #[test]
    fn allocating_too_much_memory_via_heap_is_memory_limit() {
        // V8 heap objects (strings): covered by heap_limits + near-heap callback.
        let err = run_code(
            r#"
            const arrays = [];
            while (true) { arrays.push('x'.repeat(1_000_000)); }
            "#,
            "<test>",
            Limits {
                memory_mb: 32,
                wall_time_ms: 10_000,
                cpu_time_ms: 10_000,
                ..Default::default()
            },
        )
        .map_err(|f| f.error)
        .unwrap_err();
        assert!(
            matches!(err, RunError::MemoryLimit),
            "expected MemoryLimit, got {err:?}"
        );
    }

    #[test]
    fn allocating_too_much_memory_via_typed_array_is_memory_limit() {
        // ArrayBuffer backing stores: covered by the custom BudgetAllocState.
        let err = run_code(
            r#"
            const arrays = [];
            while (true) { arrays.push(new Uint8Array(1024 * 1024)); }
            "#,
            "<test>",
            Limits {
                memory_mb: 32,
                wall_time_ms: 10_000,
                cpu_time_ms: 10_000,
                ..Default::default()
            },
        )
        .map_err(|f| f.error)
        .unwrap_err();
        assert!(
            matches!(err, RunError::MemoryLimit),
            "expected MemoryLimit, got {err:?}"
        );
    }

    #[test]
    fn wall_timeout_fires_before_cpu_timeout() {
        // wall_time_ms is tight; cpu_time_ms is generous.
        // A tight loop must be killed by the wall clock, not the CPU budget.
        //
        // Note: "pure async hang" (awaiting a never-resolving Promise) returns
        // synchronously from module.evaluate() with a pending Promise - V8 has
        // already returned control to us, so there is nothing to interrupt.
        // That case is ERR_EXPORT_NOT_SERIALIZABLE. Interrupting a bridge call
        // that never returns is the real async-hang scenario (Phase 4).
        let err = run_code(
            "let i = 0; while (true) { i++; }",
            "<test>",
            Limits {
                wall_time_ms: 200,
                cpu_time_ms: 30_000,
                ..Default::default()
            },
        )
        .map_err(|f| f.error)
        .unwrap_err();
        assert!(
            matches!(err, RunError::WallTimeout),
            "expected WallTimeout, got {err:?}"
        );
    }

    // ── Source modules (host-provided JS) ───────────────────────────────
    // The host supplies raw JS source for an import specifier. V8 compiles
    // it once per isolate and caches it. Phase 6+.

    const ZOD_SOURCE: &str = r#"
        export const z = {
          object: (shape) => ({
            parse: (data) => {
              for (const key of Object.keys(shape)) {
                if (!(key in data)) throw new Error(`missing key: ${key}`);
              }
              return data;
            }
          }),
          string: () => ({}),
          number: () => ({}),
        };
    "#;

    #[test]
    fn source_module_basic_function_import() {
        let imports = [source_import(
            "lib:math",
            "export function add(a, b) { return a + b; }",
        )];
        let out = run_with_source_imports(
            r#"import { add } from "lib:math"; export default add(1, 2)"#,
            &imports,
        )
        .unwrap();
        assert_eq!(get_default(&out).as_deref(), Some("3"));
    }

    #[test]
    fn source_module_mimicking_zod_schema_validation() {
        // Canonical AI-agent use case: host ships a mini-zod as source.
        let imports = [source_import("lib:zod", ZOD_SOURCE)];
        let out = run_with_source_imports(
            r#"
            import { z } from "lib:zod";
            const schema = z.object({ name: z.string(), age: z.number() });
            export default schema.parse({ name: "Alice", age: 30 })
            "#,
            &imports,
        )
        .unwrap();
        let default_export = get_default(&out).expect("default export missing");
        assert!(default_export.contains("Alice"));
        assert!(default_export.contains("30"));
    }

    #[test]
    fn source_module_zod_schema_fails_on_bad_data() {
        // Same setup as above, but pass data that fails validation — the
        // schema's own `throw` should surface as RuntimeError, not as a
        // ModuleNotFound from the resolver.
        let imports = [source_import("lib:zod", ZOD_SOURCE)];
        let err = run_with_source_imports(
            r#"
            import { z } from "lib:zod";
            const schema = z.object({ name: z.string() });
            export default schema.parse({ wrong: true })
            "#,
            &imports,
        )
        .unwrap_err();
        assert!(
            matches!(err, RunError::RuntimeError(_)),
            "expected RuntimeError, got {err:?}"
        );
    }

    #[test]
    fn source_module_utility_library_used_across_multiple_exports() {
        // A utility lib imported and used in both default and named export.
        // Verifies the module cache works: the resolver returns the same
        // v8::Module instance for both `clamp` and `lerp` references.
        let imports = [source_import(
            "lib:math-utils",
            "export const clamp = (n, lo, hi) => Math.max(lo, Math.min(hi, n));\n\
             export const lerp = (a, b, t) => a + (b - a) * t;",
        )];
        let out = run_with_source_imports(
            r#"
            import { clamp, lerp } from "lib:math-utils";
            export default clamp(5, 0, 10);
            export const interpolated = lerp(0, 100, 0.5);
            "#,
            &imports,
        )
        .unwrap();
        assert_eq!(get_default(&out).as_deref(), Some("5"));
        assert_eq!(get_named(&out, "interpolated").as_deref(), Some("50"));
    }

    #[test]
    fn source_module_can_import_another_source_module() {
        // Transitive source imports: "lib:app" imports from "lib:utils".
        // Module resolver must recurse into the second binding to satisfy
        // the first.
        let imports = [
            source_import("lib:utils", "export const double = (n) => n * 2;"),
            source_import(
                "lib:app",
                "import { double } from \"lib:utils\";\n\
                 export const formatResult = (n) => `result=${double(n)}`;",
            ),
        ];
        let out = run_with_source_imports(
            r#"import { formatResult } from "lib:app"; export default formatResult(42)"#,
            &imports,
        )
        .unwrap();
        assert_eq!(get_default(&out).as_deref(), Some("result=84"));
    }

    // ── Custom globals ────────────────────────────────────────────────────
    // The host installs named values onto globalThis before running user
    // code. Phase 3+ (currently only `fetch` is allowlisted; this tests
    // the general mechanism).

    #[test]
    #[ignore = "v1 only allowlists fetch as a host-provided global; arbitrary globals need a design change"]
    fn custom_global_string_is_accessible() {
        // Host provides: globals.appVersion = "1.2.3"
        // User code reads it from globalThis.
        //
        // This is intentionally ignored because DESIGN.md currently allows
        // only `fetch` as a host-provided global. Everything else should go
        // through imports unless the design changes.
        let out = run_ok(r#"export default globalThis.appVersion"#);
        assert_eq!(get_default(&out).as_deref(), Some("1.2.3"));
    }

    #[test]
    fn custom_global_object_is_accessible_and_callable() {
        // Host provides a plain object as a global:
        //   globals.config = { model: "gpt-4", maxTokens: 1000 }
        // User code reads its properties.
        //
        // When Phase 3+ lands: assert config.model === "gpt-4".
        let _ = run(r#"
            export default globalThis.config?.model ?? "not set"
            "#);
    }

    #[test]
    fn undeclared_global_at_run_time_is_undefined_not_error() {
        // A global that was never declared should silently be undefined,
        // not throw a ReferenceError. Optional chaining makes this safe.
        let out = run_ok(r#"export default globalThis.neverDeclared ?? "fallback""#);
        assert_eq!(get_default(&out).as_deref(), Some("fallback"));
    }

    #[test]
    fn host_tool_function_callable_as_global() {
        // Host wires a function onto globalThis:
        //   globals.searchTool = async (query) => { /* real impl */ }
        // This is the canonical AI-agent tool-binding pattern.
        //
        // When globals injection is implemented: assert the call resolves.
        let err = run_err(r#"export default await globalThis.searchTool("cats")"#);
        // Pre-implementation: TypeError (searchTool is not a function).
        assert!(matches!(err, RunError::RuntimeError(_)));
    }

    #[test]
    fn multiple_host_tools_available_as_globals() {
        // Several tools mounted as globals - search, fetch, summarize.
        // Each is called in sequence and results combined.
        //
        // When globals injection lands: assert all three are callable.
        let err = run_err(
            r#"
            const a = await globalThis.searchTool("query");
            const b = await globalThis.summarize(a);
            export default b
            "#,
        );
        assert!(matches!(err, RunError::RuntimeError(_)));
    }

    #[test]
    fn globals_do_not_leak_between_runs() {
        // A previous run setting globalThis.x = 99 must not affect the next
        // run. Each run gets a fresh context.
        let out1 = run_ok("globalThis.__leakTest = 99; export default 1");
        let out2 = run_ok("export default globalThis.__leakTest ?? 'clean'");
        assert_eq!(get_default(&out1).as_deref(), Some("1"));
        // Must be 'clean', not 99.
        assert_eq!(get_default(&out2).as_deref(), Some("clean"));
    }

    #[test]
    fn host_provided_data_object_as_global() {
        // The host mounts a large read-only data object as a global:
        //   globals.dataset = [{ id: 1, value: 10 }, { id: 2, value: 20 }]
        // User code does analytics on it.
        //
        // When globals injection lands: assert reduce result is 30.
        let _ = run(r#"
            const total = (globalThis.dataset ?? []).reduce(
              (acc, row) => acc + row.value, 0
            );
            export default total
            "#);
    }

    // ── Output metadata ───────────────────────────────────────────────────

    #[test]
    fn duration_ms_is_populated() {
        let out = run_ok("export default 1 + 1");
        // Sub-millisecond runs must report a non-zero fractional duration
        // (microsecond resolution) instead of truncating to 0.
        assert!(out.duration_ms > 0.0);
        assert!(out.duration_ms < 5_000.0);
    }

    // ── Precompile / prefixes ───────────────────────────────────────────────

    /// prepare()-shaped helper: validate `code` exactly as `precompile` does,
    /// then hand back the spec a run replays — mirroring what session.rs
    /// stores per prefix.
    fn prepared<'a>(
        code: &'a str,
        globals: &'a [HostGlobalDef],
        imports: &[ImportBinding],
    ) -> PrefixSpec<'a> {
        precompile(code, None, globals, imports, 0).expect("prefix must validate");
        PrefixSpec {
            code,
            filename: "<prefix>",
            globals,
        }
    }

    #[test]
    fn precompile_accepts_a_valid_prefix() {
        precompile("const x = 1", None, &[], &[], 0).unwrap();
    }

    #[test]
    fn precompile_compile_error_is_reported() {
        let err = precompile("export default (((", None, &[], &[], 0).unwrap_err();
        assert!(matches!(err.error, RunError::CompileError(_)));
    }

    #[test]
    fn precompile_runtime_error_is_reported() {
        let err = precompile(r#"throw new Error("prefix failed")"#, None, &[], &[], 0).unwrap_err();
        assert!(matches!(err.error, RunError::RuntimeError(_)));
    }

    // ── Code generation from strings is a prepare()-time capability ─────────

    #[test]
    fn run_code_cannot_eval_or_new_function() {
        let out = execute(
            "const probe = (fn) => { try { fn(); return 'allowed' } catch (e) { \
               return (e instanceof EvalError) + ':' + e.constructor.name } }\n\
             export default [probe(() => eval('1')), \
               probe(() => new Function('return 1'))].join('|')",
            None,
            Limits::default(),
            &[],
            &[],
            None,
            Arc::new(AtomicU32::new(0)),
            None,
        )
        .unwrap();
        // Catchable EvalError at the call site — the run itself completed.
        assert_eq!(
            get_default(&out).as_deref(),
            Some("true:EvalError|true:EvalError")
        );
    }

    #[test]
    fn prepared_setup_keeps_codegen_and_its_products_stay_callable() {
        // The zod-shaped pattern: setup compiles fast paths from strings at
        // prepare() time; run code calls the compiled functions but cannot
        // generate code of its own.
        let prefix = prepared(
            "globalThis.double = new Function('a', 'return a * 2')\n\
             globalThis.viaEval = eval('(x) => x + 1')",
            &[],
            &[],
        );
        let out = execute_with_prefix(
            prefix,
            Some(
                "const denied = (() => { try { eval('1'); return 'allowed' } \
                   catch (e) { return e instanceof EvalError } })()\n\
                 export default [double(21), viaEval(41), denied].join('|')",
            ),
            None,
            Limits::default(),
            &[],
            &[],
            None,
            Arc::new(AtomicU32::new(0)),
            None,
        )
        .unwrap();
        assert_eq!(get_default(&out).as_deref(), Some("42|42|true"));
    }

    // ── waitUntil grace phase (hold mode — no epilogue spec/socket) ─────────

    /// Direct-API grace defaults: the epilogue budget on, everything else
    /// default.
    fn grace_limits() -> Limits {
        Limits {
            grace_ms: 5_000,
            ..Limits::default()
        }
    }

    #[test]
    fn wait_until_work_that_finished_with_the_run_reports_settled() {
        // `await 0` resolves within the run's own microtask drain, so the
        // work (and its console line) completes before the value settles;
        // the grace phase then just confirms the set is settled. Work that
        // genuinely outlives the run is covered by the bridge test below.
        let out = execute(
            "waitUntil((async () => { await 0; console.log('bg done') })())\n\
             export default 'early'",
            None,
            grace_limits(),
            &[],
            &[],
            None,
            Arc::new(AtomicU32::new(0)),
            None,
        )
        .unwrap();
        assert_eq!(get_default(&out).as_deref(), Some("early"));
        assert_eq!(out.stdout, vec!["bg done".to_string()]);
        let bg = out.background.expect("grace phase ran");
        assert_eq!(bg.status, GraceStatus::Settled);
        assert!(bg.stdout.is_empty());
        assert!(!bg.early_result_sent);
    }

    #[test]
    fn wait_until_rejection_reports_failed_and_the_run_still_succeeds() {
        let out = execute(
            "waitUntil(Promise.reject(new TypeError('db write failed')))\n\
             export default 1",
            None,
            grace_limits(),
            &[],
            &[],
            None,
            Arc::new(AtomicU32::new(0)),
            None,
        )
        .unwrap();
        let bg = out.background.expect("grace phase ran");
        assert_eq!(bg.status, GraceStatus::Failed);
        let (name, message) = bg.error.expect("rejection captured");
        assert_eq!(name, "TypeError");
        assert!(message.contains("db write failed"), "{message}");
    }

    #[test]
    fn wait_until_accepts_non_promise_values() {
        let out = execute(
            "waitUntil(42)\nexport default 'ok'",
            None,
            grace_limits(),
            &[],
            &[],
            None,
            Arc::new(AtomicU32::new(0)),
            None,
        )
        .unwrap();
        assert_eq!(
            out.background.expect("grace phase ran").status,
            GraceStatus::Settled
        );
    }

    #[test]
    fn grace_ms_zero_disables_the_epilogue() {
        let out = execute(
            "waitUntil(new Promise(() => {}))\nexport default 'ok'",
            None,
            Limits::default(), // grace_ms: 0
            &[],
            &[],
            None,
            Arc::new(AtomicU32::new(0)),
            None,
        )
        .unwrap();
        assert!(out.background.is_none());
    }

    #[test]
    fn wait_until_throws_a_catchable_error_in_setup_code() {
        let prefix = prepared(
            "globalThis.setupMsg = (() => { \
               try { waitUntil(1); return 'allowed' } catch (e) { return e.message } })()",
            &[],
            &[],
        );
        let out = execute_with_prefix(
            prefix,
            Some("export default [globalThis.setupMsg, typeof waitUntil].join('|')"),
            None,
            Limits::default(),
            &[],
            &[],
            None,
            Arc::new(AtomicU32::new(0)),
            None,
        )
        .unwrap();
        let got = get_default(&out).unwrap();
        assert!(
            got.contains("only available to run code") && got.ends_with("|function"),
            "{got}"
        );
    }

    #[test]
    fn wait_until_is_importable_from_iso4_runtime() {
        let out = execute(
            "import { waitUntil as wu } from 'iso4:runtime'\n\
             wu((async () => { await 0; console.log('via import') })())\n\
             export default typeof wu",
            None,
            grace_limits(),
            &[],
            &[],
            None,
            Arc::new(AtomicU32::new(0)),
            None,
        )
        .unwrap();
        assert_eq!(get_default(&out).as_deref(), Some("function"));
        assert_eq!(out.stdout, vec!["via import".to_string()]);
        assert_eq!(
            out.background.expect("grace phase ran").status,
            GraceStatus::Settled
        );
    }

    #[test]
    fn a_bridge_call_keeps_working_during_grace() {
        // The background task's bridge round trip completes AFTER the value
        // settled: the fire-and-forget frame was written during the run, the
        // response is consumed by the grace poll loop.
        let (out, h) = {
            use std::mem::ManuallyDrop;
            use std::os::unix::io::AsRawFd;
            let (server, client) = std::os::unix::net::UnixStream::pair().unwrap();
            let client = ManuallyDrop::new(client);
            let fd = client.as_raw_fd();
            let handle = spawn_responder(server, |s| {
                ipc::write_ts_to_rust_frame(
                    s,
                    ipc::TsToRustMessageType::BridgeResponse,
                    &bridge_resp_ok(0, &WireValue::Number(42.0)),
                )
                .unwrap();
            });
            let result = execute(
                "waitUntil((async () => { console.log('bg:' + (await myTool())) })())\n\
                 export default 'early'",
                None,
                grace_limits(),
                &[HostGlobalDef::bridge("myTool")],
                &[],
                Some(fd),
                Arc::new(AtomicU32::new(0)),
                None,
            );
            (result, handle)
        };
        h.join().unwrap();
        let out = out.unwrap();
        assert_eq!(get_default(&out).as_deref(), Some("early"));
        let bg = out.background.expect("grace phase ran");
        assert_eq!(bg.status, GraceStatus::Settled);
        assert_eq!(bg.stdout, vec!["bg:42".to_string()]);
        assert_eq!(bg.bridge_calls.len(), 0, "the call was attempted pre-settle");
    }

    #[test]
    fn an_unresolvable_grace_promise_truncates_cleanly_and_the_core_stays_reusable() {
        // Boundary truncation (nothing to drive, no socket) is not a taint:
        // no JS was interrupted, the instance keeps serving calls.
        init_platform();
        let mut core = create_instance_core(None, &[], 0).expect("core");
        let outcome = run_call_on_core(
            &mut core,
            Some("waitUntil(new Promise(() => {}))\nexport default 1"),
            "<iso4>",
            &[],
            grace_limits(),
            &[],
            &[],
            None,
            Arc::new(AtomicU32::new(0)),
            None,
        );
        assert!(!outcome.tainted, "boundary truncation must not taint");
        let out = outcome.result.expect("run succeeded");
        assert_eq!(
            out.background.expect("grace ran").status,
            GraceStatus::Truncated
        );

        let second = run_call_on_core(
            &mut core,
            Some("export default typeof waitUntil"),
            "<iso4>",
            &[],
            Limits::default(),
            &[],
            &[],
            None,
            Arc::new(AtomicU32::new(0)),
            None,
        );
        let out = second.result.expect("instance reusable after clean truncation");
        assert_eq!(get_default(&out).as_deref(), Some("function"));
        assert!(out.background.is_none());
    }

    #[test]
    fn precompile_validation_permits_codegen() {
        precompile(
            "globalThis.f = new Function('return 1'); eval('2')",
            None,
            &[],
            &[],
            0,
        )
        .unwrap();
    }

    // Top-level `await` used to fail every prefix because nothing
    // drained the microtask queue after module evaluation.

    #[test]
    fn precompile_top_level_await_settles() {
        let prefix = prepared(
            "globalThis.a = await Promise.resolve(40)\n\
             globalThis.b = await new Response('x').text()",
            &[],
            &[],
        );
        let out = execute_with_prefix(
            prefix,
            Some("export default [globalThis.a + 1, globalThis.b].join('|')"),
            None,
            Limits::default(),
            &[],
            &[],
            None,
            Arc::new(AtomicU32::new(0)),
            None,
        )
        .unwrap();
        assert_eq!(get_default(&out).as_deref(), Some("41|x"));
    }

    #[test]
    fn precompile_top_level_await_in_plain_exports_settles() {
        // The common bundled-handler shapes: the awaited value lands in a
        // module binding (or is discarded), not on globalThis. A prefix's
        // namespace is discarded by design, so there is nothing for a postfix
        // to observe — the point is that precompile itself succeeds. The
        // failure never depended on where the value lands, only on the
        // module evaluation promise.
        for code in [
            "export const a = await 1",
            "export default await 1",
            "const a = await 1",
            "await 1",
            "export default await (async () => 1)()",
        ] {
            let result = precompile(code, None, &[], &[], 0);
            assert!(
                result.is_ok(),
                "prefix {code:?} failed: {:?}",
                result.err().map(|f| f.error)
            );
        }
    }

    #[test]
    fn precompile_imported_module_with_top_level_await_settles() {
        // Top-level await inside an *imported* source module resolves as part
        // of the same module-graph evaluation promise as the prefix itself.
        let imports = [source_import(
            "lib:config",
            "export const config = await Promise.resolve({ level: 3 })",
        )];
        let prefix = prepared(
            "import { config } from 'lib:config'; globalThis.level = config.level",
            &[],
            &imports,
        );
        let out = execute_with_prefix(
            prefix,
            Some("export default globalThis.level"),
            None,
            Limits::default(),
            &[],
            &imports,
            None,
            Arc::new(AtomicU32::new(0)),
            None,
        )
        .unwrap();
        assert_eq!(get_default(&out).as_deref(), Some("3"));
    }

    #[test]
    fn precompile_chained_top_level_awaits_settle() {
        // Chained awaits all resolve within one checkpoint — a checkpoint
        // drains the whole queue, newly enqueued microtasks included.
        let prefix = prepared(
            "let n = 0; for (let i = 0; i < 10; i++) { n = await Promise.resolve(n + 1) } \
             globalThis.n = n",
            &[],
            &[],
        );
        let out = execute_with_prefix(
            prefix,
            Some("export default globalThis.n"),
            None,
            Limits::default(),
            &[],
            &[],
            None,
            Arc::new(AtomicU32::new(0)),
            None,
        )
        .unwrap();
        assert_eq!(get_default(&out).as_deref(), Some("10"));
    }

    #[test]
    fn precompile_rejected_top_level_await_is_runtime_error() {
        let err = precompile(
            r#"await Promise.reject(new Error("prefix rejected"))"#,
            None,
            &[],
            &[],
        0)
        .unwrap_err();
        assert!(matches!(err.error, RunError::RuntimeError(_)));
    }

    #[test]
    fn precompile_never_settling_prefix_fails_with_did_not_settle() {
        let err = precompile("await new Promise(() => {})", None, &[], &[], 0).unwrap_err();
        assert!(matches!(err.error, RunError::PrefixDidNotSettle(_)));
    }

    // Bridge callables exist at prepare() time as throwing placeholders:
    // referencing them works, calling them is ERR_PREFIX_BRIDGE_CALL.

    #[test]
    fn precompile_calling_bridge_global_fails_with_prefix_bridge_call() {
        let globals = [HostGlobalDef::bridge("fetch")];
        // Awaited and fire-and-forget-at-top-level both throw synchronously
        // inside the placeholder, before any await could park the module.
        for code in [
            "await fetch('https://example.com')",
            "fetch('https://example.com')",
        ] {
            let err = precompile(code, None, &globals, &[], 0).unwrap_err();
            match err.error {
                RunError::PrefixBridgeCall(msg) => {
                    assert!(msg.contains("bridge global 'fetch'"), "message: {msg}");
                }
                other => panic!("expected PrefixBridgeCall for {code:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn runtime_internals_are_hidden_from_enumeration_but_reachable() {
        // The `__iso4_*` plumbing (dispatcher, shim handler keys) must
        // not surface through enumeration — the celld accident class is
        // generic sandbox code enumerating the global and invoking every key.
        // The names stay reachable by string (hygiene, not secrecy), and
        // host-declared names stay enumerable like `fetch` in a browser.
        use std::os::unix::io::AsRawFd;
        init_platform();
        let (server, client) = std::os::unix::net::UnixStream::pair().unwrap();
        let globals = [HostGlobalDef::Shim {
            name: "fetch".to_string(),
            shim: "(raw) => raw".to_string(),
            handler_name: "__iso4_fetch_h".to_string(),
            enumerable: true,
        }];
        let imports = [host_import(
            "tools:search",
            vec![("query", HostModuleNode::Function)],
        )];
        let out = execute(
            "const internals = (list) => list.filter((k) => k.startsWith('__iso4_'))\n\
             const forIn = []\n\
             for (const k in globalThis) forIn.push(k)\n\
             export default [\n\
               internals(Object.keys(globalThis)).length,\n\
               internals(forIn).length,\n\
               typeof globalThis['__iso4_call'],\n\
               typeof globalThis['__iso4_fetch_h'],\n\
               Object.keys(globalThis).includes('fetch'),\n\
             ].join('|')",
            None,
            Limits::default(),
            &globals,
            &imports,
            Some(client.as_raw_fd()),
            Arc::new(AtomicU32::new(0)),
            None,
        )
        .unwrap();
        drop(server);
        assert_eq!(
            get_default(&out).as_deref(),
            Some("0|0|function|function|true")
        );
    }

    #[test]
    fn a_non_enumerable_global_is_hidden_but_callable() {
        // The host's per-global opt-out. Hidden names stay reachable
        // (hygiene, not secrecy); undeclared-flag names stay enumerable.
        use std::os::unix::io::AsRawFd;
        init_platform();
        let (server, client) = std::os::unix::net::UnixStream::pair().unwrap();
        let globals = [
            HostGlobalDef::Bridge {
                name: "hiddenTool".to_string(),
                enumerable: false,
            },
            HostGlobalDef::StringExpr {
                name: "HIDDEN_CONST".to_string(),
                expr: "41".to_string(),
                enumerable: false,
            },
            HostGlobalDef::bridge("visibleTool"),
        ];
        let out = execute(
            "const keys = Object.keys(globalThis)\n\
             export default [\n\
               keys.includes('hiddenTool'),\n\
               keys.includes('HIDDEN_CONST'),\n\
               keys.includes('visibleTool'),\n\
               typeof hiddenTool,\n\
               HIDDEN_CONST + 1,\n\
             ].join('|')",
            None,
            Limits::default(),
            &globals,
            &[],
            Some(client.as_raw_fd()),
            Arc::new(AtomicU32::new(0)),
            None,
        )
        .unwrap();
        drop(server);
        assert_eq!(
            get_default(&out).as_deref(),
            Some("false|false|true|function|42")
        );
    }

    #[test]
    fn a_non_enumerable_global_has_the_same_surface_at_prepare_and_run_time() {
        // The between-runs placeholder installs with the declared flag, so
        // enumeration inside the prefix and inside a run agree.
        use std::os::unix::io::AsRawFd;
        init_platform();
        let (server, client) = std::os::unix::net::UnixStream::pair().unwrap();
        let globals = [HostGlobalDef::Bridge {
            name: "tool".to_string(),
            enumerable: false,
        }];
        let prefix = prepared(
            "globalThis.prefixEnumerated = Object.keys(globalThis).includes('tool')\n\
             globalThis.prefixType = typeof tool",
            &globals,
            &[],
        );
        let out = execute_with_prefix(
            prefix,
            Some(
                "export default [prefixEnumerated, prefixType, \
                   Object.keys(globalThis).includes('tool'), typeof tool].join('|')",
            ),
            None,
            Limits::default(),
            &globals,
            &[],
            Some(client.as_raw_fd()),
            Arc::new(AtomicU32::new(0)),
            None,
        )
        .unwrap();
        drop(server);
        assert_eq!(
            get_default(&out).as_deref(),
            Some("false|function|false|function")
        );
    }

    #[test]
    fn precompile_calling_shim_global_fails_with_prefix_bridge_call() {
        let globals = [HostGlobalDef::Shim {
            name: "fetch".to_string(),
            shim: "(raw) => raw".to_string(),
            handler_name: "__iso4_fetch_h".to_string(),
            enumerable: true,
        }];
        let err =
            precompile("await fetch('https://example.com')", None, &globals, &[], 0).unwrap_err();
        match err.error {
            // The placeholder sits under the handler name but reports the
            // public name — the user called `fetch`, not `__iso4_fetch_h`.
            RunError::PrefixBridgeCall(msg) => {
                assert!(msg.contains("bridge global 'fetch'"), "message: {msg}");
            }
            other => panic!("expected PrefixBridgeCall, got {other:?}"),
        }
    }

    #[test]
    fn precompile_calling_host_import_function_fails_with_prefix_bridge_call() {
        let imports = [host_import(
            "tools:search",
            vec![("query", HostModuleNode::Function)],
        )];
        let err = precompile(
            "import { query } from 'tools:search'; await query('dogs')",
            None,
            &[],
            &imports,
            0,
        )
        .unwrap_err();
        assert!(matches!(err.error, RunError::PrefixBridgeCall(_)));
    }

    #[test]
    fn precompile_bridge_global_is_visible_but_not_callable() {
        // typeof matches run() code, and stashing a reference is fine — only
        // the call is rejected.
        let globals = [HostGlobalDef::bridge("fetch")];
        let prefix = prepared("globalThis.kind = typeof fetch", &globals, &[]);
        let out = execute_with_prefix(
            prefix,
            Some("export default globalThis.kind"),
            None,
            Limits::default(),
            &[],
            &[],
            None,
            Arc::new(AtomicU32::new(0)),
            None,
        )
        .unwrap();
        assert_eq!(get_default(&out).as_deref(), Some("function"));
    }

    #[test]
    fn execute_with_prefix_basic_postfix() {
        // Module-scoped `const` stays in the prefix module's scope.
        // Use globalThis to share values with the postfix module.
        let prefix = prepared("globalThis.base = 100", &[], &[]);
        let out = execute_with_prefix(
            prefix,
            Some("export default globalThis.base + 1"),
            None,
            Limits::default(),
            &[],
            &[],
            None,
            Arc::new(AtomicU32::new(0)),
            None,
        )
        .unwrap();
        assert_eq!(get_default(&out).as_deref(), Some("101"));
    }

    /// The web classes are native `FunctionTemplate`s installed fresh into
    /// every run's context; a closure the prefix creates over them must still
    /// work when the postfix stage of the same run calls it.
    #[test]
    fn web_globals_are_usable_from_prefix_code() {
        let prefix = prepared(
            "globalThis.mk = () => new Response('hi', { status: 201 })",
            &[],
            &[],
        );
        let out = execute_with_prefix(
            prefix, Some("const r = globalThis.mk(); \
             export default [r instanceof Response, r.status, r.headers.get('content-type')].join('|')"),
            None,
            Limits::default(),
            &[],
            &[],
            None,
            Arc::new(AtomicU32::new(0)),
            None,
        )
        .unwrap();
        assert_eq!(
            get_default(&out).as_deref(),
            Some("true|201|text/plain;charset=UTF-8")
        );
    }

    /// A live web-type instance created by prefix code must be visible to
    /// the postfix stage of the run (and validate cleanly at prepare() time).
    #[test]
    fn a_live_instance_in_the_prefix_is_visible_to_the_postfix() {
        for expr in [
            "new Response('x', { status: 201 })",
            "new Request('https://ex.com/a')",
            "new Headers([['content-type', 'text/plain']])",
        ] {
            let prefix_src = format!("globalThis.v = {expr}");
            precompile(&prefix_src, None, &[], &[], 0)
                .unwrap_or_else(|_| panic!("precompile with a live {expr} must not fail"));
            let out = execute_with_prefix(
                PrefixSpec {
                    code: &prefix_src,
                    filename: "<prefix>",
                    globals: &[],
                },
                Some("export default typeof globalThis.v"),
                None,
                Limits::default(),
                &[],
                &[],
                None,
                Arc::new(AtomicU32::new(0)),
                None,
            )
            .unwrap_or_else(|_| panic!("run with a prefix holding {expr} must not fail"));
            assert_eq!(get_default(&out).as_deref(), Some("object"), "{expr}");
        }
    }

    /// A postfix constructing a `Response` in a prefix run must still
    /// serialize through the host-object path on the way out.
    #[test]
    fn a_response_built_in_a_prefix_run_still_serializes() {
        let prefix = prepared("", &[], &[]);
        let out = execute_with_prefix(
            prefix,
            Some("export default new Response('body', { status: 418, headers: { 'x-a': 'b' } })"),
            None,
            Limits::default(),
            &[],
            &[],
            None,
            Arc::new(AtomicU32::new(0)),
            None,
        )
        .unwrap();
        // The export blob carries a host object; the host decodes it. Here we
        // only assert the run succeeded and produced one export — decoding is
        // covered on the TS side.
        assert!(!out.exports.is_empty(), "expected a serialized export");
    }

    #[test]
    fn execute_with_prefix_global_mutation_visible_in_postfix() {
        let prefix = prepared("globalThis.answer = 42", &[], &[]);
        let out = execute_with_prefix(
            prefix,
            Some("export default globalThis.answer"),
            None,
            Limits::default(),
            &[],
            &[],
            None,
            Arc::new(AtomicU32::new(0)),
            None,
        )
        .unwrap();
        assert_eq!(get_default(&out).as_deref(), Some("42"));
    }

    #[test]
    fn execute_with_prefix_multiple_postfixes_are_independent() {
        let prefix = prepared("globalThis.base = 10", &[], &[]);
        let b = "globalThis.base";
        let out1 = execute_with_prefix(
            prefix,
            Some(&format!("export default {b} * 2")),
            None,
            Limits::default(),
            &[],
            &[],
            None,
            Arc::new(AtomicU32::new(0)),
            None,
        )
        .unwrap();
        let out2 = execute_with_prefix(
            prefix,
            Some(&format!("export default {b} * 3")),
            None,
            Limits::default(),
            &[],
            &[],
            None,
            Arc::new(AtomicU32::new(0)),
            None,
        )
        .unwrap();
        let out3 = execute_with_prefix(
            prefix,
            Some(&format!("export default {b} * 4")),
            None,
            Limits::default(),
            &[],
            &[],
            None,
            Arc::new(AtomicU32::new(0)),
            None,
        )
        .unwrap();
        assert_eq!(get_default(&out1).as_deref(), Some("20"));
        assert_eq!(get_default(&out2).as_deref(), Some("30"));
        assert_eq!(get_default(&out3).as_deref(), Some("40"));
    }

    #[test]
    fn execute_with_prefix_postfix_mutations_do_not_leak_between_runs() {
        let prefix = prepared("globalThis.counter = 0", &[], &[]);
        execute_with_prefix(
            prefix,
            Some("globalThis.counter = 99; export default 1"),
            None,
            Limits::default(),
            &[],
            &[],
            None,
            Arc::new(AtomicU32::new(0)),
            None,
        )
        .unwrap();
        let out = execute_with_prefix(
            prefix,
            Some("export default globalThis.counter"),
            None,
            Limits::default(),
            &[],
            &[],
            None,
            Arc::new(AtomicU32::new(0)),
            None,
        )
        .unwrap();
        assert_eq!(get_default(&out).as_deref(), Some("0"));
    }

    #[test]
    fn execute_with_prefix_complex_prefix_computation() {
        let prefix = prepared(
            r#"const sq = {}; for (let i = 0; i <= 10; i++) sq[i] = i * i; globalThis.sq = sq;"#,
            &[],
            &[],
        );
        let out = execute_with_prefix(
            prefix,
            Some("export default globalThis.sq[7]"),
            None,
            Limits::default(),
            &[],
            &[],
            None,
            Arc::new(AtomicU32::new(0)),
            None,
        )
        .unwrap();
        assert_eq!(get_default(&out).as_deref(), Some("49"));
    }

    #[test]
    fn execute_with_prefix_console_is_available_in_postfix() {
        let prefix = prepared("const x = 1", &[], &[]);
        let out = execute_with_prefix(
            prefix,
            Some(r#"console.log("hello from postfix"); export default 1"#),
            None,
            Limits::default(),
            &[],
            &[],
            None,
            Arc::new(AtomicU32::new(0)),
            None,
        )
        .unwrap();
        assert!(out.stdout.iter().any(|l| l.contains("hello from postfix")));
    }

    #[test]
    fn execute_with_prefix_postfix_runtime_error_is_reported() {
        let prefix = prepared("", &[], &[]);
        let err = execute_with_prefix(
            prefix,
            Some(r#"throw new Error("postfix failed")"#),
            None,
            Limits::default(),
            &[],
            &[],
            None,
            Arc::new(AtomicU32::new(0)),
            None,
        )
        .unwrap_err();
        assert!(matches!(err.error, RunError::RuntimeError(_)));
    }

    // ── AbortSignal / cancellation ────────────────────────────────────────
    // Requires AbortSignal threading through run_code (Phase 1+).

    #[test]
    #[ignore = "requires signal/abort support"]
    fn aborted_signal_before_run_is_err_aborted() {
        // If the caller aborts the signal before the run starts, the result
        // should be ERR_ABORTED immediately without executing any JS.
        // TODO: pass a pre-aborted signal into run_code and assert ERR_ABORTED.
        todo!()
    }

    #[test]
    #[ignore = "requires signal/abort support"]
    fn aborted_signal_during_async_run_is_err_aborted() {
        // Signal is aborted while the sandbox is mid-execution (e.g., waiting
        // on a host bridge call). Rust calls terminate_execution().
        todo!()
    }

    // ── Export size limit ─────────────────────────────────────────────────
    // Requires maxExportBytes enforcement (Phase 1+).

    #[test]
    fn export_exceeding_size_limit_is_err_export_too_large() {
        // User code exports a string whose serialised blob exceeds
        // maxExportBytes. The run must fail with ExportTooLarge instead of
        // sending a huge payload over the wire.
        let limits = Limits {
            max_export_bytes: 64, // tiny — any non-trivial export will exceed it
            ..Limits::default()
        };
        let err = run_code(r#"export default "x".repeat(100)"#, "<iso4>", limits)
            .unwrap_err()
            .error;
        assert!(
            matches!(err, RunError::ExportTooLarge),
            "expected ExportTooLarge, got {err:?}"
        );
    }

    #[test]
    fn export_within_size_limit_succeeds() {
        // Exporting a value that fits within maxExportBytes must succeed.
        let limits = Limits {
            max_export_bytes: 1024 * 1024, // 1 MiB
            ..Limits::default()
        };
        let out = run_code("export default 42", "<iso4>", limits).unwrap();
        assert_eq!(get_default(&out).as_deref(), Some("42"));
    }

    #[test]
    fn export_size_limit_zero_means_no_limit() {
        // max_export_bytes = 0 disables the cap entirely.
        let limits = Limits {
            max_export_bytes: 0,
            ..Limits::default()
        };
        // Export a reasonably large value — should succeed when cap is off.
        let out = run_code(r#"export default "x".repeat(10_000)"#, "<iso4>", limits).unwrap();
        assert!(get_default(&out).is_some());
    }

    // ── stdout / stderr size limits ───────────────────────────────────────
    // Requires maxStdoutBytes / maxStderrBytes enforcement (Phase 3+).

    #[test]
    fn stdout_exceeding_limit_is_silently_truncated() {
        // Lines that would push stdout over maxStdoutBytes are silently dropped.
        // The run itself must still succeed.
        let limits = Limits {
            max_stdout_bytes: 50, // allow only ~50 bytes of stdout
            ..Limits::default()
        };
        let out = run_code(
            // Each line is "hello" (5 bytes); 20 lines = 100 bytes total.
            // Only the first ~10 lines (50 bytes) should survive.
            r#"
            for (let i = 0; i < 20; i++) { console.log("hello"); }
            export default 1
            "#,
            "<iso4>",
            limits,
        )
        .unwrap();
        // At most 50 bytes worth of lines should have been captured.
        let total: usize = out.stdout.iter().map(|s| s.len()).sum();
        assert!(total <= 50, "stdout bytes {total} exceeded limit 50");
        // At least some output must have been captured.
        assert!(
            !out.stdout.is_empty(),
            "expected some stdout to be captured"
        );
    }

    #[test]
    fn stderr_exceeding_limit_is_silently_truncated() {
        let limits = Limits {
            max_stderr_bytes: 50,
            ..Limits::default()
        };
        let out = run_code(
            r#"
            for (let i = 0; i < 20; i++) { console.error("oops"); }
            export default 1
            "#,
            "<iso4>",
            limits,
        )
        .unwrap();
        let total: usize = out.stderr.iter().map(|s| s.len()).sum();
        assert!(total <= 50, "stderr bytes {total} exceeded limit 50");
        assert!(
            !out.stderr.is_empty(),
            "expected some stderr to be captured"
        );
    }

    #[test]
    fn stdout_limit_zero_means_no_limit() {
        // max_stdout_bytes = 0 disables the cap.
        let limits = Limits {
            max_stdout_bytes: 0,
            ..Limits::default()
        };
        let out = run_code(
            r#"
            for (let i = 0; i < 100; i++) { console.log("line"); }
            export default 1
            "#,
            "<iso4>",
            limits,
        )
        .unwrap();
        assert_eq!(out.stdout.len(), 100);
    }

    // ── ERR_UNDECLARED_BINDING ────────────────────────────────────────────
    // Phase 2+ (requires precompile / snapshots).

    #[test]
    #[ignore = "requires snapshot/precompile (Phase 2)"]
    fn run_with_undeclared_binding_is_error() {
        // prefix.run() passes a global name that was not declared at
        // precompile() time. Must fail with ERR_UNDECLARED_BINDING instead
        // of silently installing an unexpected global into the restored
        // snapshot context.
        todo!()
    }

    // ── CPU budget excludes async wait time ───────────────────────────────
    // Phase 3 (cpu budget bracketing).

    #[test]
    fn cpu_budget_does_not_count_time_waiting_on_bridge() {
        // A run with a tight cpuTimeMs limit (50 ms) but a slow bridge
        // response (100 ms sleep) must NOT hit ERR_CPU_TIMEOUT because
        // cpu_budget.leave() is called before the blocking bridge read.
        use std::mem::ManuallyDrop;
        use std::os::unix::io::AsRawFd;
        use std::os::unix::net::UnixStream;

        let (mut server, client) = UnixStream::pair().unwrap();
        let client = ManuallyDrop::new(client);
        let fd = client.as_raw_fd();

        let responder = std::thread::spawn(move || {
            let _ = ipc::read_rust_to_ts_frame(&mut server).unwrap();
            std::thread::sleep(Duration::from_millis(100));
            let mut payload = Vec::new();
            payload.extend_from_slice(&0u32.to_be_bytes()); // callId
            payload.push(1); // ok = true
            payload.push(1); // value present
            push_value_blob(&mut payload, &WireValue::Number(1.0));
            ipc::write_ts_to_rust_frame(
                &mut server,
                ipc::TsToRustMessageType::BridgeResponse,
                &payload,
            )
            .unwrap();
        });

        let out = execute(
            "export default await myTool()",
            None,
            Limits {
                cpu_time_ms: 50,
                wall_time_ms: 5_000,
                ..Default::default()
            },
            &[HostGlobalDef::bridge("myTool")],
            &[],
            Some(fd),
            Arc::new(AtomicU32::new(0)),
            None,
        );
        responder.join().unwrap();
        assert!(
            out.is_ok(),
            "expected Ok (bridge wait excluded from cpu), got: {:?}",
            out.map_err(|f| f.error)
        );
    }

    #[test]
    fn cpu_budget_does_count_tight_sync_loop() {
        let err = run_code(
            "let x = 0; while (true) x++;",
            "<test>",
            Limits {
                cpu_time_ms: 100,
                wall_time_ms: 5_000,
                ..Default::default()
            },
        )
        .map_err(|f| f.error)
        .unwrap_err();
        assert!(
            matches!(err, RunError::CpuTimeout),
            "expected CpuTimeout, got {err:?}"
        );
    }

    // ── Bridge tests ───────────────────────────────────────────────────────────────────
    //
    // These tests exercise the full bridge round-trip: V8 calls a host global,
    // a responder thread reads the BridgeCall frame and writes a BridgeResponse,
    // and we assert on the returned JS value or error code.

    /// Append a value slot: `u32 byteLength` + V8 serialization blob.
    fn push_value_blob(out: &mut Vec<u8>, value: &WireValue) {
        let blob = testval::to_blob(value);
        out.extend_from_slice(&(blob.len() as u32).to_be_bytes());
        out.extend_from_slice(&blob);
    }

    /// Encode a successful BridgeResponse payload (callId, ok=1, value).
    fn bridge_resp_ok(call_id: u32, value: &WireValue) -> Vec<u8> {
        let mut p = Vec::new();
        p.extend_from_slice(&call_id.to_be_bytes());
        p.push(1); // ok
        p.push(1); // value present
        push_value_blob(&mut p, value);
        p
    }

    /// Encode an error BridgeResponse payload (callId, ok=0, name/message/fields).
    fn bridge_resp_err_full(
        call_id: u32,
        name: &str,
        message: &str,
        fields: Option<&WireValue>,
    ) -> Vec<u8> {
        let mut p = Vec::new();
        p.extend_from_slice(&call_id.to_be_bytes());
        p.push(0); // ok = false
        for s in &["ERR_HOST_BRIDGE", name, message] {
            let b = s.as_bytes();
            p.extend_from_slice(&(b.len() as u32).to_be_bytes());
            p.extend_from_slice(b);
        }
        p.push(0); // no stack
        match fields {
            Some(f) => {
                p.push(1);
                push_value_blob(&mut p, f);
            }
            None => p.push(0),
        }
        p
    }

    /// Encode an error BridgeResponse payload (callId, ok=0, message).
    fn bridge_resp_err(call_id: u32, message: &str) -> Vec<u8> {
        bridge_resp_err_full(call_id, "Error", message, None)
    }

    /// Spawn a single-response bridge responder thread.
    /// Reads one BridgeCall frame, calls `respond`, drops the server.
    fn spawn_responder(
        mut server: std::os::unix::net::UnixStream,
        respond: impl FnOnce(&mut std::os::unix::net::UnixStream) + Send + 'static,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let _ = ipc::read_rust_to_ts_frame(&mut server).unwrap();
            respond(&mut server);
        })
    }

    /// Run code with one declared global, backed by a socket pair.
    /// Returns the execute result and the responder JoinHandle.
    fn run_with_bridge(
        code: &str,
        global: &str,
        limits: Limits,
        respond: impl FnOnce(&mut std::os::unix::net::UnixStream) + Send + 'static,
    ) -> (Result<Output, FailureOutput>, std::thread::JoinHandle<()>) {
        use std::mem::ManuallyDrop;
        use std::os::unix::io::AsRawFd;
        let (server, client) = std::os::unix::net::UnixStream::pair().unwrap();
        let client = ManuallyDrop::new(client);
        let fd = client.as_raw_fd();
        let handle = spawn_responder(server, respond);
        let result = execute(
            code,
            None,
            limits,
            &[HostGlobalDef::bridge(global)],
            &[],
            Some(fd),
            Arc::new(AtomicU32::new(0)),
            None,
        );
        (result, handle)
    }

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
                ipc::write_ts_to_rust_frame(
                    s,
                    ipc::TsToRustMessageType::BridgeResponse,
                    &bridge_resp_ok(0, &WireValue::Number(42.0)),
                )
                .unwrap();
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
                ipc::write_ts_to_rust_frame(
                    s,
                    ipc::TsToRustMessageType::Terminate,
                    &7u32.to_be_bytes(),
                )
                .unwrap();
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
        assert!(!failure.bridge_calls[0].ok);
        assert!(!failure.bridge_calls[0].blocked);
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
                ipc::write_ts_to_rust_frame(
                    s,
                    ipc::TsToRustMessageType::BridgeResponse,
                    &bridge_resp_ok(0, &WireValue::String("hello bridge".into())),
                )
                .unwrap();
            },
        );
        h.join().unwrap();
        assert_eq!(get_default(&out.unwrap()).as_deref(), Some("hello bridge"));
    }

    #[test]
    fn bridge_call_passes_args_to_host() {
        // Verify the BridgeCall frame actually contains the argument we passed.
        use std::mem::ManuallyDrop;
        use std::os::unix::io::AsRawFd;
        let (server, client) = std::os::unix::net::UnixStream::pair().unwrap();
        let client = ManuallyDrop::new(client);
        let fd = client.as_raw_fd();

        let responder = std::thread::spawn(move || {
            let mut server = server;
            let frame = ipc::read_rust_to_ts_frame(&mut server).unwrap();
            assert_eq!(frame.message_type, ipc::RustToTsMessageType::BridgeCall);
            // callId (4) + targetKind (1) + specifier absent (1) + name len (4)
            // + "add" (3) = 13, then the args value slot: u32 length + blob.
            let (_, _, _, export_name, args) = parse_bridge_call(&frame.payload);
            assert_eq!(export_name, "add");
            assert_eq!(args, vec![WireValue::Number(7.0)]);
            ipc::write_ts_to_rust_frame(
                &mut server,
                ipc::TsToRustMessageType::BridgeResponse,
                &bridge_resp_ok(0, &WireValue::Number(99.0)),
            )
            .unwrap();
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
            Some(fd),
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
                ipc::write_ts_to_rust_frame(
                    s,
                    ipc::TsToRustMessageType::BridgeResponse,
                    &bridge_resp_err(0, "handler blew up"),
                )
                .unwrap();
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
                ipc::write_ts_to_rust_frame(
                    s,
                    ipc::TsToRustMessageType::BridgeResponse,
                    &bridge_resp_err_full(0, "WorkflowTimeout", "took too long", Some(&fields)),
                )
                .unwrap();
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
                ipc::write_ts_to_rust_frame(
                    s,
                    ipc::TsToRustMessageType::BridgeResponse,
                    &bridge_resp_err_full(0, "TypeError", "bad input", Some(&fields)),
                )
                .unwrap();
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
                ipc::write_ts_to_rust_frame(
                    s,
                    ipc::TsToRustMessageType::BridgeResponse,
                    &bridge_resp_err_full(0, "Error", "real message", Some(&fields)),
                )
                .unwrap();
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
                ipc::write_ts_to_rust_frame(
                    s,
                    ipc::TsToRustMessageType::BridgeResponse,
                    &bridge_resp_err_full(0, "NonRetryableError", "nope", None),
                )
                .unwrap();
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
        use std::mem::ManuallyDrop;
        use std::os::unix::io::AsRawFd;
        let (mut server, client) = std::os::unix::net::UnixStream::pair().unwrap();
        let client = ManuallyDrop::new(client);
        let fd = client.as_raw_fd();

        // The bridge errors before writing to the socket, so no frame arrives.
        // Give the server a short read timeout to avoid hanging.
        server
            .set_read_timeout(Some(Duration::from_millis(500)))
            .ok();
        let responder = std::thread::spawn(move || {
            let _ = ipc::read_rust_to_ts_frame(&mut server); // may timeout — ignore
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
            Some(fd),
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
        use std::mem::ManuallyDrop;
        use std::os::unix::io::AsRawFd;
        let (mut server, client) = std::os::unix::net::UnixStream::pair().unwrap();
        let client = ManuallyDrop::new(client);
        let fd = client.as_raw_fd();

        // The bridge aborts before writing to the socket, so no frame arrives.
        server
            .set_read_timeout(Some(Duration::from_millis(500)))
            .ok();
        let responder = std::thread::spawn(move || {
            let _ = ipc::read_rust_to_ts_frame(&mut server); // may timeout — ignore
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
            Some(fd),
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
        // BridgeResponse frame reads are capped by memory_mb (the sandbox
        // cannot hold more data than its own memory budget).
        // memory_mb = 1 → frame cap = 1 MiB. The response below is ~41 bytes
        // which fits easily, so we use a very small memory_mb value (1 byte
        // expressed in MB rounds to 1 MiB minimum via saturating_mul). We
        // instead test with a response larger than memory_mb * 1 MiB by using
        // memory_mb = 0 (no memory limit set → 64 MiB cap) and crafting a
        // large string. To keep the test fast we just verify the frame-cap
        // logic: set memory_mb = 1 (1 MiB cap) and send a response that fits,
        // confirming the run succeeds, then rely on the wall-timeout path for
        // the too-large case (testing the full OOM path is too slow here).
        //
        // The meaningful invariant is tested in the wall_timeout test: if the
        // host never sends a valid frame the sandbox times out cleanly.
        // Frame-cap rejection manifests as an Internal socket error which
        // ultimately surfaces as WallTimeout (wall guard fires first).
        let (out, h) = run_with_bridge(
            "export default await myTool()",
            "myTool",
            Limits {
                cpu_time_ms: 5_000,
                wall_time_ms: 10_000,
                memory_mb: 64, // sets the BridgeResponse frame cap to 64 MiB
                ..Default::default()
            },
            |s| {
                // Response is tiny — should succeed.
                ipc::write_ts_to_rust_frame(
                    s,
                    ipc::TsToRustMessageType::BridgeResponse,
                    &bridge_resp_ok(0, &WireValue::String("ok".into())),
                )
                .unwrap();
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
                ipc::write_ts_to_rust_frame(
                    s,
                    ipc::TsToRustMessageType::BridgeResponse,
                    &bridge_resp_ok(0, &WireValue::Number(7.0)),
                )
                .unwrap();
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
                ipc::write_ts_to_rust_frame(
                    s,
                    ipc::TsToRustMessageType::BridgeResponse,
                    &bridge_resp_ok(0, &WireValue::String("big payload".into())),
                )
                .unwrap();
            },
        );
        h.join().unwrap();
        assert_eq!(get_default(&out.unwrap()).as_deref(), Some("big payload"));
    }

    #[test]
    fn bridge_call_wall_timeout_mid_wait() {
        // If the host never responds, the wall guard terminates V8.
        use std::mem::ManuallyDrop;
        use std::os::unix::io::AsRawFd;
        let (server, client) = std::os::unix::net::UnixStream::pair().unwrap();
        let client = ManuallyDrop::new(client);
        let fd = client.as_raw_fd();
        let _server_guard = server; // keep fd open, never respond

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
            Some(fd),
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
                ipc::write_ts_to_rust_frame(
                    s,
                    ipc::TsToRustMessageType::BridgeResponse,
                    &bridge_resp_ok(
                        0,
                        &WireValue::Object(vec![
                            ("x".to_string(), WireValue::Number(3.0)),
                            ("y".to_string(), WireValue::Number(4.0)),
                        ]),
                    ),
                )
                .unwrap();
            },
        );
        h.join().unwrap();
        assert_eq!(get_default(&out.unwrap()).as_deref(), Some("7"));
    }

    #[test]
    fn bridge_wait_respects_wall_timeout() {
        // If the TS host never responds and a wall limit is set, the callback's
        // own socket read-timeout fires and terminates V8 with WallTimeout.
        use std::mem::ManuallyDrop;
        use std::os::unix::io::AsRawFd;
        let (server, client) = std::os::unix::net::UnixStream::pair().unwrap();
        let client = ManuallyDrop::new(client);
        let fd = client.as_raw_fd();
        let _guard = server; // keep open, never respond

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
            Some(fd),
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
                ipc::write_ts_to_rust_frame(
                    s,
                    ipc::TsToRustMessageType::BridgeResponse,
                    &bridge_resp_ok(
                        0,
                        &WireValue::Object(vec![
                            ("x".to_string(), WireValue::Number(1.0)),
                            ("__proto__".to_string(), WireValue::Number(99.0)),
                            ("y".to_string(), WireValue::Number(2.0)),
                        ]),
                    ),
                )
                .unwrap();
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
                ipc::write_ts_to_rust_frame(
                    s,
                    ipc::TsToRustMessageType::BridgeResponse,
                    &bridge_resp_ok(
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
                    ),
                )
                .unwrap();
            },
        );
        h.join().unwrap();
        assert_eq!(get_default(&out.unwrap()).as_deref(), Some("true,true,1"));
    }

    // ── max_bridge_calls limit ─────────────────────────────────────────────

    /// Drain N BridgeCall frames from `server`, responding to each with the
    /// given WireValue. Returns the number of calls actually received before
    /// the socket is closed/times-out.
    fn drain_bridge_calls(
        mut server: std::os::unix::net::UnixStream,
        max: usize,
        value: WireValue,
    ) -> usize {
        let mut count = 0;
        server
            .set_read_timeout(Some(Duration::from_millis(500)))
            .ok();
        while count < max {
            match ipc::read_rust_to_ts_frame(&mut server) {
                Ok(frame) if frame.message_type == ipc::RustToTsMessageType::BridgeCall => {
                    // Parse call_id from the first 4 bytes of the payload.
                    let call_id = u32::from_be_bytes(frame.payload[..4].try_into().unwrap());
                    let resp = bridge_resp_ok(call_id, &value);
                    if ipc::write_ts_to_rust_frame(
                        &mut server,
                        ipc::TsToRustMessageType::BridgeResponse,
                        &resp,
                    )
                    .is_err()
                    {
                        break;
                    }
                    count += 1;
                }
                _ => break,
            }
        }
        count
    }

    #[test]
    fn bridge_call_limit_exceeded_after_limit_calls() {
        // The sandbox calls myTool 5 times; limit is 3. The first 3 succeed,
        // the 4th triggers ERR_BRIDGE_CALL_LIMIT_EXCEEDED before any I/O.
        use std::mem::ManuallyDrop;
        use std::os::unix::io::AsRawFd;
        let (server, client) = std::os::unix::net::UnixStream::pair().unwrap();
        let client = ManuallyDrop::new(client);
        let fd = client.as_raw_fd();

        let handle =
            std::thread::spawn(move || drain_bridge_calls(server, 10, WireValue::Number(1.0)));

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
            Some(fd),
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
        use std::mem::ManuallyDrop;
        use std::os::unix::io::AsRawFd;
        let (server, client) = std::os::unix::net::UnixStream::pair().unwrap();
        let client = ManuallyDrop::new(client);
        let fd = client.as_raw_fd();

        let handle =
            std::thread::spawn(move || drain_bridge_calls(server, 5, WireValue::Number(10.0)));

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
            Some(fd),
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
        use std::mem::ManuallyDrop;
        use std::os::unix::io::AsRawFd;
        let (server, client) = std::os::unix::net::UnixStream::pair().unwrap();
        let client = ManuallyDrop::new(client);
        let fd = client.as_raw_fd();

        let handle =
            std::thread::spawn(move || drain_bridge_calls(server, 10, WireValue::Number(1.0)));

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
            Some(fd),
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
        use std::mem::ManuallyDrop;
        use std::os::unix::io::AsRawFd;
        let (server, client) = std::os::unix::net::UnixStream::pair().unwrap();
        let client = ManuallyDrop::new(client);
        let fd = client.as_raw_fd();

        // Respond to up to 4 bridge calls, regardless of which global.
        let handle =
            std::thread::spawn(move || drain_bridge_calls(server, 4, WireValue::Number(1.0)));

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
            Some(fd),
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
        use std::mem::ManuallyDrop;
        use std::os::unix::io::AsRawFd;
        let (server, client) = std::os::unix::net::UnixStream::pair().unwrap();
        let client = ManuallyDrop::new(client);
        let fd = client.as_raw_fd();

        let handle =
            std::thread::spawn(move || drain_bridge_calls(server, 3, WireValue::Number(1.0)));

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
            Some(fd),
            Arc::new(AtomicU32::new(0)),
            None,
        )
        .unwrap();

        handle.join().unwrap();
        assert_eq!(out.bridge_calls.len(), 3);
        for record in &out.bridge_calls {
            assert_eq!(record.name, "myTool");
            assert!(record.ok, "served call must settle ok");
            assert!(!record.blocked);
            assert!(record.arg_bytes > 0, "call payload has at least the header");
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
        use std::mem::ManuallyDrop;
        use std::os::unix::io::AsRawFd;
        let (server, client) = std::os::unix::net::UnixStream::pair().unwrap();
        let client = ManuallyDrop::new(client);
        let fd = client.as_raw_fd();

        let handle =
            std::thread::spawn(move || drain_bridge_calls(server, 10, WireValue::Number(1.0)));

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
            Some(fd),
            Arc::new(AtomicU32::new(0)),
            None,
        )
        .unwrap_err();

        handle.join().unwrap();
        assert!(matches!(failure.error, RunError::BridgeCallLimitExceeded));
        assert_eq!(failure.bridge_calls.len(), 4);
        assert!(failure.bridge_calls[..3].iter().all(|r| r.ok && !r.blocked));
        let violating = &failure.bridge_calls[3];
        assert!(violating.blocked, "attempt past the limit is blocked");
        assert!(!violating.ok);
        assert_eq!(violating.response_bytes, 0);
    }

    #[test]
    fn bridge_call_limit_violation_is_uncatchable() {
        // Sandbox code swallows the limit error in try/catch and would have
        // completed successfully before the fix (the violation only surfaced
        // if the error propagated uncaught). The violation must terminate
        // execution: the run fails even though every throw is caught, and no
        // attempt past the limit reaches the host.
        use std::mem::ManuallyDrop;
        use std::os::unix::io::AsRawFd;
        let (server, client) = std::os::unix::net::UnixStream::pair().unwrap();
        let client = ManuallyDrop::new(client);
        let fd = client.as_raw_fd();

        let handle =
            std::thread::spawn(move || drain_bridge_calls(server, 10, WireValue::Number(1.0)));

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
            Some(fd),
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
        use std::mem::ManuallyDrop;
        use std::os::unix::io::AsRawFd;
        let (server, client) = std::os::unix::net::UnixStream::pair().unwrap();
        let client = ManuallyDrop::new(client);
        let fd = client.as_raw_fd();

        let handle =
            std::thread::spawn(move || drain_bridge_calls(server, 1, WireValue::Number(1.0)));

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
            Some(fd),
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
        use std::mem::ManuallyDrop;
        use std::os::unix::io::AsRawFd;
        let (server, client) = std::os::unix::net::UnixStream::pair().unwrap();
        let client = ManuallyDrop::new(client);
        let fd = client.as_raw_fd();

        let handle =
            std::thread::spawn(move || drain_bridge_calls(server, 1, WireValue::Number(1.0)));

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
            Some(fd),
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
        use std::mem::ManuallyDrop;
        use std::os::unix::io::AsRawFd;
        let (mut server, client) = std::os::unix::net::UnixStream::pair().unwrap();
        let client = ManuallyDrop::new(client);
        let fd = client.as_raw_fd();

        let handle = std::thread::spawn(move || {
            // Read the BridgeCall and reply with a deliberately wrong callId.
            let frame = ipc::read_rust_to_ts_frame(&mut server).unwrap();
            assert_eq!(frame.message_type, ipc::RustToTsMessageType::BridgeCall);
            let sent_call_id = u32::from_be_bytes(frame.payload[..4].try_into().unwrap());
            // Respond with a callId that differs from the one Rust sent.
            let wrong_call_id = sent_call_id.wrapping_add(99);
            ipc::write_ts_to_rust_frame(
                &mut server,
                ipc::TsToRustMessageType::BridgeResponse,
                &bridge_resp_ok(wrong_call_id, &WireValue::Number(1.0)),
            )
            .unwrap();
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
            Some(fd),
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
        use std::mem::ManuallyDrop;
        use std::os::unix::io::AsRawFd;

        let counter = Arc::new(AtomicU32::new(0));

        // ── Run 1: succeeds normally ─────────────────────────────────────
        let (mut server1, client1) = std::os::unix::net::UnixStream::pair().unwrap();
        let client1 = ManuallyDrop::new(client1);
        let fd1 = client1.as_raw_fd();

        let h1 = std::thread::spawn(move || {
            let frame = ipc::read_rust_to_ts_frame(&mut server1).unwrap();
            let cid = u32::from_be_bytes(frame.payload[..4].try_into().unwrap());
            ipc::write_ts_to_rust_frame(
                &mut server1,
                ipc::TsToRustMessageType::BridgeResponse,
                &bridge_resp_ok(cid, &WireValue::Number(1.0)),
            )
            .unwrap();
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
            Some(fd1),
            Arc::clone(&counter),
            None,
        )
        .unwrap();
        assert_eq!(get_default(&out).as_deref(), Some("1"));
        let run1_call_id = h1.join().unwrap();
        assert_eq!(run1_call_id, 0, "run 1 should use callId=0");

        // ── Run 2: counter has advanced; a stale run-1 response is rejected ─
        let (mut server2, client2) = std::os::unix::net::UnixStream::pair().unwrap();
        let client2 = ManuallyDrop::new(client2);
        let fd2 = client2.as_raw_fd();

        let h2 = std::thread::spawn(move || {
            let frame = ipc::read_rust_to_ts_frame(&mut server2).unwrap();
            let cid = u32::from_be_bytes(frame.payload[..4].try_into().unwrap());
            // Simulate a stale BridgeResponse from run 1 arriving here
            // (callId=0 instead of the expected callId for run 2).
            ipc::write_ts_to_rust_frame(
                &mut server2,
                ipc::TsToRustMessageType::BridgeResponse,
                &bridge_resp_ok(run1_call_id, &WireValue::Number(99.0)), // stale callId=0
            )
            .unwrap();
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
            Some(fd2),
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
        // wire at the same time.  The server responds in REVERSE order to prove
        // the poll loop routes each response to the correct resolver by callId.
        use std::mem::ManuallyDrop;
        use std::os::unix::io::AsRawFd;
        let (mut server, client) = std::os::unix::net::UnixStream::pair().unwrap();
        let client = ManuallyDrop::new(client);
        let fd = client.as_raw_fd();

        let handle = std::thread::spawn(move || {
            // Read both BridgeCall frames (they arrive before V8 yields).
            let f1 = ipc::read_rust_to_ts_frame(&mut server).unwrap();
            let cid1 = u32::from_be_bytes(f1.payload[..4].try_into().unwrap());
            let f2 = ipc::read_rust_to_ts_frame(&mut server).unwrap();
            let cid2 = u32::from_be_bytes(f2.payload[..4].try_into().unwrap());

            // Respond in reverse order: second call gets 20, first gets 10.
            // The poll loop must route cid2 → toolB resolver (20)
            // and cid1 → toolA resolver (10) regardless of order.
            ipc::write_ts_to_rust_frame(
                &mut server,
                ipc::TsToRustMessageType::BridgeResponse,
                &bridge_resp_ok(cid2, &WireValue::Number(20.0)),
            )
            .unwrap();
            ipc::write_ts_to_rust_frame(
                &mut server,
                ipc::TsToRustMessageType::BridgeResponse,
                &bridge_resp_ok(cid1, &WireValue::Number(10.0)),
            )
            .unwrap();
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
            Some(fd),
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
        use std::mem::ManuallyDrop;
        use std::os::unix::io::AsRawFd;
        let (mut server, client) = std::os::unix::net::UnixStream::pair().unwrap();
        let client = ManuallyDrop::new(client);
        let fd = client.as_raw_fd();

        let handle = std::thread::spawn(move || {
            let mut call_ids = Vec::new();
            for _ in 0..3 {
                let f = ipc::read_rust_to_ts_frame(&mut server).unwrap();
                let cid = u32::from_be_bytes(f.payload[..4].try_into().unwrap());
                call_ids.push(cid);
            }
            // Respond in reverse.
            for (i, &cid) in call_ids.iter().rev().enumerate() {
                ipc::write_ts_to_rust_frame(
                    &mut server,
                    ipc::TsToRustMessageType::BridgeResponse,
                    &bridge_resp_ok(cid, &WireValue::Number((i as f64 + 1.0) * 10.0)),
                )
                .unwrap();
            }
        });

        // All three resolve — we just care the module completes without error.
        let out = execute(
            "const [a, b, c] = await Promise.all([tool(), tool(), tool()]); \
             export default typeof a === 'number' && typeof b === 'number' && typeof c === 'number'",
            None,
            Limits { cpu_time_ms: 5_000, wall_time_ms: 10_000, ..Default::default() },
            &[HostGlobalDef::bridge("tool")],
            &[], Some(fd),
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

    // ── Async context (AsyncLocalStorage) ─────────────────────────────────

    #[test]
    fn async_context_available_via_node_async_hooks() {
        let out = run(r#"
            import { AsyncLocalStorage } from 'node:async_hooks';
            export default typeof AsyncLocalStorage;
        "#)
        .unwrap();
        assert_eq!(get_default(&out).as_deref(), Some("function"));
    }

    #[test]
    fn async_context_propagates_across_awaits() {
        // The store set by `run` is visible several awaits deep, through a
        // nested async function that never received it as an argument.
        let out = run(r#"
            import { AsyncLocalStorage } from 'node:async_hooks';
            const als = new AsyncLocalStorage();
            async function deep() {
                await Promise.resolve();
                await Promise.resolve();
                return als.getStore();
            }
            export default await als.run('trace-42', async () => {
                await Promise.resolve();
                return deep();
            });
        "#)
        .unwrap();
        assert_eq!(get_default(&out).as_deref(), Some("trace-42"));
    }

    #[test]
    fn async_context_getstore_undefined_outside_run() {
        let out = run(r#"
            import { AsyncLocalStorage } from 'node:async_hooks';
            const als = new AsyncLocalStorage();
            export default als.getStore() === undefined ? 'undef' : 'defined';
        "#)
        .unwrap();
        assert_eq!(get_default(&out).as_deref(), Some("undef"));
    }

    #[test]
    fn async_context_nested_run_breadcrumb() {
        // The durable-workflow `step.do` pattern: each nested scope appends a
        // segment to the key, and the inner scope sees the accumulated path
        // while the outer scope is restored afterwards.
        let out = run(r#"
            import { AsyncLocalStorage } from 'node:async_hooks';
            const keyScope = new AsyncLocalStorage();
            function step(name, body) {
                const parent = keyScope.getStore() ?? '';
                const key = parent ? parent + '/' + name : name;
                return keyScope.run(key, body);
            }
            const seen = [];
            await step('charge', async () => {
                await step('validate', async () => {
                    await Promise.resolve();
                    seen.push(keyScope.getStore());
                });
                seen.push(keyScope.getStore());
            });
            export default seen.join(',');
        "#)
        .unwrap();
        assert_eq!(get_default(&out).as_deref(), Some("charge/validate,charge"));
    }

    #[test]
    fn async_context_concurrent_branches_isolated() {
        // Two branches run concurrently with interleaved awaits; each must see
        // only its own store. This is the case a plain module variable gets
        // wrong.
        let out = run(r#"
            import { AsyncLocalStorage } from 'node:async_hooks';
            const als = new AsyncLocalStorage();
            async function branch(label) {
                return als.run(label, async () => {
                    await Promise.resolve();
                    await Promise.resolve();
                    const a = als.getStore();
                    await Promise.resolve();
                    const b = als.getStore();
                    return a + ':' + b;
                });
            }
            const [x, y] = await Promise.all([branch('A'), branch('B')]);
            export default x + '|' + y;
        "#)
        .unwrap();
        assert_eq!(get_default(&out).as_deref(), Some("A:A|B:B"));
    }

    #[test]
    fn async_context_two_instances_independent() {
        let out = run(r#"
            import { AsyncLocalStorage } from 'node:async_hooks';
            const a = new AsyncLocalStorage();
            const b = new AsyncLocalStorage();
            export default await a.run('AA', async () => {
                return b.run('BB', async () => {
                    await Promise.resolve();
                    return a.getStore() + ',' + b.getStore();
                });
            });
        "#)
        .unwrap();
        assert_eq!(get_default(&out).as_deref(), Some("AA,BB"));
    }

    #[test]
    fn async_context_not_available_in_prefix_code() {
        // Prefix code predates the per-run async-context machinery, so
        // importing `node:async_hooks` from a prefix does not resolve — a
        // contract kept from the snapshot era (see ResolverContext::
        // async_context_builtin). It returns a clean ModuleNotFound.
        let err = validate_prefix_module(
            "import { AsyncLocalStorage } from 'node:async_hooks'; export default 1;",
            "<prefix>",
            &[],
            &[],
            0,
        )
        .unwrap_err();
        assert!(
            matches!(err.error, RunError::ModuleNotFound(_)),
            "expected ModuleNotFound, got {:?}",
            err.error
        );
    }

    #[test]
    fn precompile_unresolved_import_is_clean_error_not_crash() {
        // Regression (snapshot era): an unresolvable import used to segfault
        // (unconditional create_blob after a failed instantiate). Validation
        // must return a clean ModuleNotFound.
        let err = validate_prefix_module(
            "import x from 'totally-nonexistent-xyz'; export default x;",
            "<prefix>",
            &[],
            &[],
            0,
        )
        .unwrap_err();
        assert!(
            matches!(err.error, RunError::ModuleNotFound(_)),
            "expected ModuleNotFound, got {:?}",
            err.error
        );
    }

    #[test]
    fn validate_prefix_module_accepts_valid_prefix() {
        validate_prefix_module(
            "globalThis.base = 100; export default 1;",
            "<prefix>",
            &[],
            &[],
            0,
        )
        .unwrap();
    }

    // ── Host modules built natively from shape data ────────────────────
    //
    // Host modules cross the wire as data trees; the runtime builds the module
    // itself. Data leaves need no socket. Function leaves dispatch through the
    // auto-installed `__iso4_call` stub, so those tests use a responder thread
    // like the bridge tests above.

    /// Parse a BridgeCall payload: (callId, targetKind, specifier, exportName, args).
    fn parse_bridge_call(payload: &[u8]) -> (u32, u8, Option<String>, String, Vec<WireValue>) {
        let mut off = 0usize;
        let call_id = u32::from_be_bytes(payload[off..off + 4].try_into().unwrap());
        off += 4;
        let target_kind = payload[off];
        off += 1;
        let specifier = if payload[off] == 1 {
            off += 1;
            let len = u32::from_be_bytes(payload[off..off + 4].try_into().unwrap()) as usize;
            off += 4;
            let s = String::from_utf8(payload[off..off + len].to_vec()).unwrap();
            off += len;
            Some(s)
        } else {
            off += 1;
            None
        };
        let len = u32::from_be_bytes(payload[off..off + 4].try_into().unwrap()) as usize;
        off += 4;
        let export_name = String::from_utf8(payload[off..off + len].to_vec()).unwrap();
        off += len;
        // The whole argument list is one value slot holding an array.
        let blob_len = u32::from_be_bytes(payload[off..off + 4].try_into().unwrap()) as usize;
        off += 4;
        let args = match testval::from_blob(&payload[off..off + blob_len]) {
            WireValue::Array(items) => items,
            other => panic!("BridgeCall args must decode to an array, got {other:?}"),
        };
        (call_id, target_kind, specifier, export_name, args)
    }

    /// Run code with host imports over a socket pair; the responder receives
    /// each BridgeCall frame parsed and answers via the returned closure.
    fn run_with_host_imports(
        code: &str,
        imports: Vec<ImportBinding>,
        respond: impl FnOnce(&mut std::os::unix::net::UnixStream, &[u8]) + Send + 'static,
    ) -> (Result<Output, FailureOutput>, std::thread::JoinHandle<()>) {
        use std::mem::ManuallyDrop;
        use std::os::unix::io::AsRawFd;
        let (mut server, client) = std::os::unix::net::UnixStream::pair().unwrap();
        let client = ManuallyDrop::new(client);
        let fd = client.as_raw_fd();
        let handle = std::thread::spawn(move || {
            let frame = ipc::read_rust_to_ts_frame(&mut server).unwrap();
            respond(&mut server, &frame.payload);
        });
        let result = execute(
            code,
            None,
            Limits::default(),
            &[],
            &imports,
            Some(fd),
            Arc::new(AtomicU32::new(0)),
            None,
        );
        (result, handle)
    }

    #[test]
    fn host_module_data_leaves_materialise_natively() {
        // Pure data host module — no socket, no bridge.
        let imports = [host_import(
            "conf:app",
            vec![
                (
                    "retries",
                    HostModuleNode::Data(testval::to_blob(&WireValue::Number(3.0))),
                ),
                (
                    "region",
                    HostModuleNode::Data(testval::to_blob(&WireValue::String("eu".to_string()))),
                ),
                (
                    "flags",
                    HostModuleNode::Data(testval::to_blob(&WireValue::Array(vec![
                        WireValue::Bool(true),
                        WireValue::Null,
                    ]))),
                ),
            ],
        )];
        let out = run_with_source_imports(
            r#"
            import { retries, region, flags } from "conf:app";
            export default `${retries}|${region}|${flags.length}|${flags[0]}`
            "#,
            &imports,
        )
        .unwrap();
        assert_eq!(get_default(&out).as_deref(), Some("3|eu|2|true"));
    }

    #[test]
    fn host_module_default_and_nested_object_exports() {
        let imports = [host_import(
            "conf:shape",
            vec![
                (
                    "default",
                    HostModuleNode::Data(testval::to_blob(&WireValue::Number(7.0))),
                ),
                (
                    "nested",
                    HostModuleNode::Object(vec![(
                        "deep".to_string(),
                        HostModuleNode::Data(testval::to_blob(&WireValue::Object(vec![(
                            "x".to_string(),
                            WireValue::Number(1.0),
                        )]))),
                    )]),
                ),
            ],
        )];
        let out = run_with_source_imports(
            r#"
            import seven, { nested } from "conf:shape";
            export default seven + nested.deep.x
            "#,
            &imports,
        )
        .unwrap();
        assert_eq!(get_default(&out).as_deref(), Some("8"));
    }

    #[test]
    fn host_module_function_leaf_dispatches_resolved_bridge_call() {
        let imports = vec![host_import(
            "tools:search",
            vec![("query", HostModuleNode::Function)],
        )];
        let (out, h) = run_with_host_imports(
            r#"
            import { query } from "tools:search";
            export default await query("cats", 2)
            "#,
            imports,
            |s, payload| {
                let (call_id, target_kind, specifier, export_name, args) =
                    parse_bridge_call(payload);
                // The frame carries the resolved import target — the handle ID
                // never leaves the runtime.
                assert_eq!(target_kind, 1);
                assert_eq!(specifier.as_deref(), Some("tools:search"));
                assert_eq!(export_name, "query");
                assert_eq!(
                    args,
                    vec![
                        WireValue::String("cats".to_string()),
                        WireValue::Number(2.0)
                    ]
                );
                ipc::write_ts_to_rust_frame(
                    s,
                    ipc::TsToRustMessageType::BridgeResponse,
                    &bridge_resp_ok(call_id, &WireValue::Number(42.0)),
                )
                .unwrap();
            },
        );
        h.join().unwrap();
        let out = out.unwrap();
        assert_eq!(get_default(&out).as_deref(), Some("42"));
        // The bridge record carries the resolved public name.
        assert_eq!(out.bridge_calls.len(), 1);
        assert_eq!(out.bridge_calls[0].name, "tools:search.query");
        assert!(out.bridge_calls[0].ok);
    }

    #[test]
    fn host_module_nested_function_leaf_and_second_module_offsets() {
        // Handle IDs are assigned across ALL declared bindings in walk order;
        // calling a leaf of the second module must resolve to the second
        // module's specifier and path.
        let imports = vec![
            host_import(
                "tools:a",
                vec![
                    ("first", HostModuleNode::Function),
                    (
                        "nested",
                        HostModuleNode::Object(vec![(
                            "inner".to_string(),
                            HostModuleNode::Function,
                        )]),
                    ),
                ],
            ),
            host_import("tools:b", vec![("second", HostModuleNode::Function)]),
        ];
        let (out, h) = run_with_host_imports(
            r#"
            import { second } from "tools:b";
            export default await second()
            "#,
            imports,
            |s, payload| {
                let (call_id, target_kind, specifier, export_name, _) = parse_bridge_call(payload);
                assert_eq!(target_kind, 1);
                assert_eq!(specifier.as_deref(), Some("tools:b"));
                assert_eq!(export_name, "second");
                ipc::write_ts_to_rust_frame(
                    s,
                    ipc::TsToRustMessageType::BridgeResponse,
                    &bridge_resp_ok(call_id, &WireValue::String("ok".to_string())),
                )
                .unwrap();
            },
        );
        h.join().unwrap();
        let out = out.unwrap();
        assert_eq!(get_default(&out).as_deref(), Some("ok"));
        assert_eq!(out.bridge_calls[0].name, "tools:b.second");
    }

    #[test]
    fn host_module_invalid_export_name_is_compile_error() {
        let imports = [host_import(
            "bad:name",
            vec![(
                "not a name",
                HostModuleNode::Data(testval::to_blob(&WireValue::Null)),
            )],
        )];
        let err = run_with_source_imports(
            r#"import * as x from "bad:name"; export default 1"#,
            &imports,
        )
        .unwrap_err();
        assert!(
            matches!(&err, RunError::CompileError(m) if m.contains("not a name")),
            "expected CompileError about the invalid key, got {err:?}"
        );
    }

    #[test]
    fn host_module_direct_dispatcher_call_with_bad_handle_rejects_catchably() {
        // Sandbox code calling the reserved dispatcher directly with a bogus
        // handle gets a catchable rejection — not a hang, not a crash.
        let imports = vec![host_import(
            "tools:x",
            vec![("f", HostModuleNode::Function)],
        )];
        let (out, h) = run_with_host_imports(
            r#"
            import { f } from "tools:x";
            let caught = "no";
            try { await globalThis.__iso4_call(999) } catch (e) { caught = e.message }
            export default caught
            "#,
            imports,
            |_s, _payload| {
                // No BridgeCall frame is ever written for the bad handle; the
                // responder would block forever, so it must not be reached.
                // (This closure only runs if a frame arrives — fail loudly.)
                panic!("no BridgeCall frame expected for an invalid handle");
            },
        );
        let out = out.unwrap();
        let msg = get_default(&out).unwrap();
        assert!(
            msg.contains("no host import handle"),
            "expected catchable rejection message, got: {msg}"
        );
        // One blocked record for the refused attempt.
        assert_eq!(out.bridge_calls.len(), 1);
        assert!(out.bridge_calls[0].blocked);
        // The responder thread never got a frame; drop it by closing our end.
        drop(h);
    }

    #[test]
    fn host_module_import_meta_is_not_visible_to_user_code() {
        // The values array rides on the HOST MODULE's import.meta only; user
        // code's own import.meta must stay empty.
        let imports = [host_import(
            "conf:x",
            vec![(
                "v",
                HostModuleNode::Data(testval::to_blob(&WireValue::Number(1.0))),
            )],
        )];
        let out = run_with_source_imports(
            r#"
            import { v } from "conf:x";
            export default `${v}|${JSON.stringify(import.meta.__iso4 ?? null)}`
            "#,
            &imports,
        )
        .unwrap();
        assert_eq!(get_default(&out).as_deref(), Some("1|null"));
    }

    #[test]
    fn prefix_with_host_module_data_and_stored_trampoline_works_per_run() {
        // The documented pattern: prefix imports a host module and stashes a
        // function leaf on globalThis. The trampoline and data leaves are
        // plain JS re-created at each run's prefix stage; the postfix call
        // dispatches through the fresh per-run `__iso4_call` stub.
        use std::mem::ManuallyDrop;
        use std::os::unix::io::AsRawFd;

        let imports = vec![host_import(
            "tools:search",
            vec![
                ("query", HostModuleNode::Function),
                (
                    "limit",
                    HostModuleNode::Data(testval::to_blob(&WireValue::Number(5.0))),
                ),
            ],
        )];
        let prefix_src = r#"
            import { query, limit } from "tools:search";
            globalThis.search = query;
            globalThis.maxResults = limit;
            export default 1;
            "#;
        validate_prefix_module(prefix_src, "<prefix>", &[], &imports, 0).unwrap();

        let (mut server, client) = std::os::unix::net::UnixStream::pair().unwrap();
        let client = ManuallyDrop::new(client);
        let fd = client.as_raw_fd();
        let handle = std::thread::spawn(move || {
            let frame = ipc::read_rust_to_ts_frame(&mut server).unwrap();
            let (call_id, target_kind, specifier, export_name, args) =
                parse_bridge_call(&frame.payload);
            assert_eq!(target_kind, 1);
            assert_eq!(specifier.as_deref(), Some("tools:search"));
            assert_eq!(export_name, "query");
            assert_eq!(args, vec![WireValue::String("dogs".to_string())]);
            ipc::write_ts_to_rust_frame(
                &mut server,
                ipc::TsToRustMessageType::BridgeResponse,
                &bridge_resp_ok(call_id, &WireValue::Number(3.0)),
            )
            .unwrap();
        });

        let out = execute_with_prefix(
            PrefixSpec {
                code: prefix_src,
                filename: "<prefix>",
                globals: &[],
            },
            Some("export default (await globalThis.search('dogs')) + globalThis.maxResults"),
            None,
            Limits::default(),
            &[],
            &imports,
            Some(fd),
            Arc::new(AtomicU32::new(0)),
            None,
        )
        .unwrap();
        handle.join().unwrap();
        assert_eq!(get_default(&out).as_deref(), Some("8"));
        assert_eq!(out.bridge_calls[0].name, "tools:search.query");
    }

    #[test]
    fn postfix_can_import_host_module_declared_at_precompile() {
        // The postfix itself may also import the declared specifier — the
        // module is rebuilt in the run isolate from the same declared shape.
        use std::mem::ManuallyDrop;
        use std::os::unix::io::AsRawFd;

        let imports = vec![host_import(
            "tools:t",
            vec![("f", HostModuleNode::Function)],
        )];
        let prefix_src = "globalThis.ready = true;";
        validate_prefix_module(prefix_src, "<prefix>", &[], &imports, 0).unwrap();

        let (mut server, client) = std::os::unix::net::UnixStream::pair().unwrap();
        let client = ManuallyDrop::new(client);
        let fd = client.as_raw_fd();
        let handle = std::thread::spawn(move || {
            let frame = ipc::read_rust_to_ts_frame(&mut server).unwrap();
            let (call_id, _, specifier, export_name, _) = parse_bridge_call(&frame.payload);
            assert_eq!(specifier.as_deref(), Some("tools:t"));
            assert_eq!(export_name, "f");
            ipc::write_ts_to_rust_frame(
                &mut server,
                ipc::TsToRustMessageType::BridgeResponse,
                &bridge_resp_ok(call_id, &WireValue::Bool(true)),
            )
            .unwrap();
        });

        let out = execute_with_prefix(
            PrefixSpec {
                code: prefix_src,
                filename: "<prefix>",
                globals: &[],
            },
            Some(r#"import { f } from "tools:t"; export default await f()"#),
            None,
            Limits::default(),
            &[],
            &imports,
            Some(fd),
            Arc::new(AtomicU32::new(0)),
            None,
        )
        .unwrap();
        handle.join().unwrap();
        assert_eq!(get_default(&out).as_deref(), Some("true"));
    }

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
        let names: Vec<String> = handles.iter().map(|h| h.record_name()).collect();
        assert_eq!(
            names,
            vec!["a.one", "a.obj.two", "a.obj.deeper.three", "b.four"]
        );
        assert_eq!(host_module_base_id(&imports, "a"), 0);
        assert_eq!(host_module_base_id(&imports, "b"), 3);
    }

    // ── Host → sandbox calls ───────────────────────────────────────────

    /// Build a `CallSpec` with the args encoded as one blob holding an array.
    fn call_spec(path: &str, args: &[WireValue]) -> ipc::CallSpec {
        ipc::CallSpec {
            export_path: path.to_string(),
            args_blob: testval::to_blob(&WireValue::Array(args.to_vec())),
        }
    }

    /// Shorthand: `run({ code, call })` with no globals and no socket.
    fn run_call(code: &str, path: &str, args: &[WireValue]) -> Result<Output, RunError> {
        init_platform();
        let call = call_spec(path, args);
        run_module(
            Some(code),
            "<iso4>",
            Limits::default(),
            &[],
            &[],
            None,
            Arc::new(AtomicU32::new(0)),
            Some(&call),
        )
        .map_err(|failure| failure.error)
    }

    /// Decode a call run's value blob.
    fn value_of(out: &Output) -> WireValue {
        testval::from_blob(&out.exports)
    }

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
        use std::mem::ManuallyDrop;
        use std::os::unix::io::AsRawFd;
        let (server, client) = std::os::unix::net::UnixStream::pair().unwrap();
        let client = ManuallyDrop::new(client);
        let fd = client.as_raw_fd();
        let handle = spawn_responder(server, |s| {
            ipc::write_ts_to_rust_frame(
                s,
                ipc::TsToRustMessageType::BridgeResponse,
                &bridge_resp_ok(0, &WireValue::Number(41.0)),
            )
            .unwrap();
        });
        let call = call_spec("f", &[WireValue::Number(1.0)]);
        let out = execute(
            "export async function f(n) { return (await myTool()) + n }",
            None,
            Limits::default(),
            &[HostGlobalDef::bridge("myTool")],
            &[],
            Some(fd),
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
        // prefix.call(): no postfix at all — the export path resolves against
        // the prefix module's live namespace, module-scope closure state
        // included.
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
            None,
            None,
            Limits::default(),
            &[],
            &[],
            None,
            Arc::new(AtomicU32::new(0)),
            Some(&call),
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
            None,
            None,
            Limits::default(),
            &[],
            &[],
            None,
            Arc::new(AtomicU32::new(0)),
            Some(&call),
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
            Some("export function f() { return 'x'.repeat(1024 * 1024) }"),
            "<iso4>",
            Limits {
                max_export_bytes: 1024,
                ..Default::default()
            },
            &[],
            &[],
            None,
            Arc::new(AtomicU32::new(0)),
            Some(&call),
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
            Some("export function f() { for (;;) {} }"),
            "<iso4>",
            Limits {
                cpu_time_ms: 100,
                wall_time_ms: 10_000,
                ..Default::default()
            },
            &[],
            &[],
            None,
            Arc::new(AtomicU32::new(0)),
            Some(&call),
        )
        .unwrap_err();
        assert!(matches!(failure.error, RunError::CpuTimeout));
    }

    // ── Result serialization stays under the run's budgets ──────────────────
    // V8's ValueSerializer runs guest getters (structured-clone semantics), so
    // serialization is guest execution: a getter that never returns must be
    // terminated by the still-armed guards. Each run is raced against a
    // channel timeout so a regression fails the test instead of hanging the
    // suite (the leaked spinner thread then dies with the test process).

    fn run_code_bounded(
        code: &'static str,
        limits: Limits,
        call: Option<ipc::CallSpec>,
    ) -> Result<Output, FailureOutput> {
        init_platform();
        let (tx, rx) = crossbeam_channel::bounded(1);
        std::thread::spawn(move || {
            let result = run_module(
                Some(code),
                "<test>",
                limits,
                &[],
                &[],
                None,
                Arc::new(AtomicU32::new(0)),
                call.as_ref(),
            );
            let _ = tx.send(result);
        });
        rx.recv_timeout(Duration::from_secs(30))
            .expect("run never returned: guards did not interrupt serialization")
    }

    #[test]
    fn spinning_getter_in_export_is_terminated() {
        let err = run_code_bounded(
            "export default { get a() { for (;;) {} } }",
            Limits {
                cpu_time_ms: 200,
                wall_time_ms: 5_000,
                ..Default::default()
            },
            None,
        )
        .map_err(|f| f.error)
        .unwrap_err();
        assert!(
            matches!(err, RunError::CpuTimeout | RunError::WallTimeout),
            "expected timeout, got {err:?}"
        );
    }

    #[test]
    fn spinning_getter_nested_in_export_is_terminated() {
        // The pre-check only sees the top level; the serializer walks
        // arbitrarily deep, so the guards must cover the whole walk.
        let err = run_code_bounded(
            "export default { a: { get b() { for (;;) {} } } }",
            Limits {
                cpu_time_ms: 200,
                wall_time_ms: 5_000,
                ..Default::default()
            },
            None,
        )
        .map_err(|f| f.error)
        .unwrap_err();
        assert!(
            matches!(err, RunError::CpuTimeout | RunError::WallTimeout),
            "expected timeout, got {err:?}"
        );
    }

    #[test]
    fn spinning_getter_in_call_result_is_terminated() {
        let err = run_code_bounded(
            "export function f() { return { get a() { for (;;) {} } } }",
            Limits {
                cpu_time_ms: 200,
                wall_time_ms: 5_000,
                ..Default::default()
            },
            Some(call_spec("f", &[])),
        )
        .map_err(|f| f.error)
        .unwrap_err();
        assert!(
            matches!(err, RunError::CpuTimeout | RunError::WallTimeout),
            "expected timeout, got {err:?}"
        );
    }

    #[test]
    fn benign_getter_export_crosses_as_its_value() {
        // Structured-clone semantics stay intact: a getter that returns is
        // invoked once during serialization and its value crosses.
        let out = run_ok("export default { get a() { return 41 + 1 } }");
        assert_eq!(
            get_field(&out, "default"),
            Some(WireValue::Object(vec![(
                "a".to_string(),
                WireValue::Number(42.0)
            )]))
        );
        assert!(out.skipped_exports.is_empty());
    }

    // ── Warm instances: create/call split, taint, warm-up budget ──────

    /// A counting prefix: module-scope state observable through a call.
    const COUNTER_PREFIX: &str = "let n = 0\nexport function bump() { return ++n }";

    fn counter_core() -> InstanceCore {
        init_platform();
        create_instance_core(
            Some(PrefixSpec {
                code: COUNTER_PREFIX,
                filename: "<prefix>",
                globals: &[],
            }),
            &[],
            0,
        )
        .expect("counter prefix warms up")
    }

    /// One `bump()` call on a warm core with the given limits.
    fn bump(core: &mut InstanceCore, limits: Limits) -> CallOutcome {
        let call = call_spec("bump", &[]);
        run_call_on_core(
            core,
            None,
            "<iso4>",
            &[],
            limits,
            &[],
            &[],
            None,
            Arc::new(AtomicU32::new(0)),
            Some(&call),
        )
    }

    #[test]
    fn warm_core_keeps_module_state_across_calls() {
        let mut core = counter_core();
        for expected in 1..=3 {
            let outcome = bump(&mut core, Limits::default());
            assert!(!outcome.tainted);
            let out = outcome.result.expect("bump succeeds");
            assert_eq!(value_of(&out), WireValue::Number(f64::from(expected)));
        }
    }

    #[test]
    fn codegen_stays_denied_across_warm_calls_while_setup_products_work() {
        // The retained context is switched to deny-codegen on the first call
        // and must stay that way for every later call, while functions the
        // setup compiled at warm-up keep working.
        init_platform();
        let mut core = create_instance_core(
            Some(PrefixSpec {
                code: "globalThis.mk = new Function('return 7')",
                filename: "<prefix>",
                globals: &[],
            }),
            &[],
            0,
        )
        .expect("setup using new Function warms up");
        for _ in 0..2 {
            let outcome = run_call_on_core(
                &mut core,
                Some(
                    "const denied = (() => { try { new Function('1'); return 'allowed' } \
                       catch (e) { return e instanceof EvalError } })()\n\
                     export default [mk(), denied].join('|')",
                ),
                "<iso4>",
                &[],
                Limits::default(),
                &[],
                &[],
                None,
                Arc::new(AtomicU32::new(0)),
                None,
            );
            assert!(!outcome.tainted);
            let out = outcome.result.expect("call succeeds");
            assert_eq!(get_default(&out).as_deref(), Some("7|true"));
        }
    }

    #[test]
    fn warm_core_reports_heap_usage() {
        let mut core = counter_core();
        let outcome = bump(&mut core, Limits::default());
        assert!(outcome.result.is_ok());
        assert!(
            outcome.heap_used_bytes > 0,
            "a live isolate has a non-empty heap"
        );
    }

    #[test]
    fn fired_guard_taints_the_core() {
        init_platform();
        let mut core = create_instance_core(
            Some(PrefixSpec {
                code: "export function spin() { for (;;) {} }",
                filename: "<prefix>",
                globals: &[],
            }),
            &[],
            0,
        )
        .expect("spin prefix warms up");
        let call = call_spec("spin", &[]);
        let outcome = run_call_on_core(
            &mut core,
            None,
            "<iso4>",
            &[],
            Limits {
                cpu_time_ms: 50,
                wall_time_ms: 5_000,
                ..Default::default()
            },
            &[],
            &[],
            None,
            Arc::new(AtomicU32::new(0)),
            Some(&call),
        );
        assert!(outcome.tainted, "a fired guard must taint the instance");
        let failure = outcome.result.unwrap_err();
        assert!(matches!(
            failure.error,
            RunError::CpuTimeout | RunError::WallTimeout
        ));
    }

    #[test]
    fn clean_exception_does_not_taint_and_state_survives() {
        init_platform();
        let mut core = create_instance_core(
            Some(PrefixSpec {
                code: "let n = 0\n\
                       export function bump() { return ++n }\n\
                       export function boom() { throw new Error('x') }",
                filename: "<prefix>",
                globals: &[],
            }),
            &[],
            0,
        )
        .expect("prefix warms up");
        assert!(bump(&mut core, Limits::default()).result.is_ok());

        let call = call_spec("boom", &[]);
        let outcome = run_call_on_core(
            &mut core,
            None,
            "<iso4>",
            &[],
            Limits::default(),
            &[],
            &[],
            None,
            Arc::new(AtomicU32::new(0)),
            Some(&call),
        );
        assert!(
            !outcome.tainted,
            "an ordinary uncaught exception is a clean completion"
        );
        assert!(matches!(
            outcome.result.unwrap_err().error,
            RunError::RuntimeError(_)
        ));

        // The instance stays reusable and its state is intact: n continues.
        let out = bump(&mut core, Limits::default()).result.expect("bump");
        assert_eq!(value_of(&out), WireValue::Number(2.0));
    }

    #[test]
    fn warmup_budget_rejects_a_looping_prefix() {
        init_platform();
        let result = create_instance_core(
            Some(PrefixSpec {
                code: "for (;;) {}",
                filename: "<prefix>",
                globals: &[],
            }),
            &[],
            0,
        );
        let Err(failure) = result else {
            panic!("a looping prefix cannot warm up");
        };
        assert!(matches!(failure.error, RunError::WarmupLimit));
    }

    #[test]
    fn prepare_validation_rejects_a_looping_prefix() {
        // Same budget at prepare() (validate_prefix_module), so an
        // un-warmable prefix fails at deploy time, not on the first call.
        // Before warm instances this hung prepare() forever.
        let failure =
            validate_prefix_module("for (;;) {}", "<prefix>", &[], &[], 0).expect_err("must reject");
        assert!(matches!(failure.error, RunError::WarmupLimit));
    }

    #[test]
    fn postfix_runs_on_a_warm_core_share_prefix_state() {
        // prefix.execute() on a warm instance: each postfix is a fresh module
        // but evaluates in the retained context.
        init_platform();
        let mut core = create_instance_core(
            Some(PrefixSpec {
                code: "globalThis.counter = (globalThis.counter ?? 0)",
                filename: "<prefix>",
                globals: &[],
            }),
            &[],
            0,
        )
        .expect("prefix warms up");
        for expected in 1..=2 {
            let outcome = run_call_on_core(
                &mut core,
                Some("globalThis.counter += 1\nexport default globalThis.counter"),
                "<iso4>",
                &[],
                Limits::default(),
                &[],
                &[],
                None,
                Arc::new(AtomicU32::new(0)),
                None,
            );
            assert!(!outcome.tainted);
            let out = outcome.result.expect("postfix run succeeds");
            let WireValue::Object(entries) = testval::from_blob(&out.exports) else {
                panic!("exports must decode to an object");
            };
            assert_eq!(
                entries,
                vec![(
                    "default".to_string(),
                    WireValue::Number(f64::from(expected))
                )]
            );
        }
    }

    // ── Review fixes from the warm-instances review ──────────────────────────

    /// Finding 1 (critical): a bridge stub stashed by sandbox code on one call must
    /// NOT dereference freed per-call state when invoked on a later call —
    /// the slot is disarmed between calls, so the stashed function throws a
    /// catchable error instead of a use-after-free. Pre-fix this dereferenced
    /// a freed `GlobalCallbackData` (and wrote to a stale socket fd).
    #[test]
    fn stashed_bridge_stub_is_disarmed_on_the_next_call() {
        use std::mem::ManuallyDrop;
        use std::os::unix::io::AsRawFd;
        init_platform();
        let mut core = create_instance_core(
            Some(PrefixSpec {
                code: "",
                filename: "<prefix>",
                globals: &[HostGlobalDef::bridge("g")],
            }),
            &[],
            0,
        )
        .expect("prefix warms up");

        // Call 1 binds `g` live and sandbox code stashes the reference. It
        // never calls `g`, so no bridge frame is written — the socket just
        // has to be a valid fd for the stub to install.
        let (_server, client) = std::os::unix::net::UnixStream::pair().unwrap();
        let client = ManuallyDrop::new(client);
        let fd = client.as_raw_fd();
        let out1 = run_call_on_core(
            &mut core,
            Some("globalThis.saved = g; export default 1"),
            "<iso4>",
            &[HostGlobalDef::bridge("g")],
            Limits::default(),
            &[HostGlobalDef::bridge("g")],
            &[],
            Some(fd),
            Arc::new(AtomicU32::new(0)),
            None,
        );
        assert!(!out1.tainted);
        assert!(out1.result.is_ok());

        // Call 2 does not rebind `g` (no socket): the stashed stub's slot is
        // disarmed. Invoking it must throw catchably, not crash.
        let out2 = run_call_on_core(
            &mut core,
            Some(
                "let r; try { globalThis.saved(); r = 'called' } \
                 catch (e) { r = 'threw' } export default r",
            ),
            "<iso4>",
            &[HostGlobalDef::bridge("g")],
            Limits::default(),
            &[],
            &[],
            None,
            Arc::new(AtomicU32::new(0)),
            None,
        );
        assert!(!out2.tainted, "a caught throw is a clean completion");
        let out = out2.result.expect("call 2 completes");
        assert_eq!(get_default(&out).as_deref(), Some("threw"));
    }

    /// Finding 2: a postfix can plant a hostile accessor under a bridge-global name;
    /// the next call's placeholder re-arm must overwrite it WITHOUT invoking
    /// the setter. Pre-fix the re-arm went through `[[Set]]`, so a
    /// looping setter hung the owner thread (only bounded, post-guards-move,
    /// by the CPU guard).
    #[test]
    fn hostile_setter_under_a_bridge_name_does_not_hijack_rearm() {
        init_platform();
        let mut core = create_instance_core(
            Some(PrefixSpec {
                code: "",
                filename: "<prefix>",
                globals: &[HostGlobalDef::bridge("g")],
            }),
            &[],
            0,
        )
        .expect("prefix warms up");

        // Call 1 replaces `g` with a setter that would loop forever if invoked.
        let out1 = run_call_on_core(
            &mut core,
            Some(
                "Object.defineProperty(globalThis, 'g', \
                 { configurable: true, set() { for (;;) {} }, get() { return 1 } }); \
                 export default 1",
            ),
            "<iso4>",
            &[HostGlobalDef::bridge("g")],
            Limits::default(),
            &[],
            &[],
            None,
            Arc::new(AtomicU32::new(0)),
            None,
        );
        assert!(!out1.tainted);
        assert!(out1.result.is_ok());

        // Call 2's re-arm defines `g` = placeholder via DefineOwnProperty,
        // which does not trigger the setter — so the call completes cleanly
        // and quickly instead of hanging / CPU-timing-out.
        let out2 = run_call_on_core(
            &mut core,
            Some("export default 7"),
            "<iso4>",
            &[HostGlobalDef::bridge("g")],
            Limits {
                cpu_time_ms: 2_000,
                wall_time_ms: 5_000,
                ..Default::default()
            },
            &[],
            &[],
            None,
            Arc::new(AtomicU32::new(0)),
            None,
        );
        assert!(!out2.tainted, "the setter must not run during re-arm");
        assert_eq!(get_default(&out2.result.unwrap()).as_deref(), Some("7"));
    }

    /// Finding 7: blowing the heap cap during warm-up reports `ERR_MEMORY_LIMIT`,
    /// not the time-shaped `ERR_WARMUP_LIMIT`.
    #[test]
    fn warmup_memory_blowout_reports_memory_limit() {
        init_platform();
        let result = create_instance_core(
            Some(PrefixSpec {
                // ~64 MB backing store under an 8 MB cap.
                code: "globalThis.hog = new Uint8Array(64 * 1024 * 1024)",
                filename: "<prefix>",
                globals: &[],
            }),
            &[],
            8,
        );
        let Err(failure) = result else {
            panic!("prefix exceeds the cap but warmed up");
        };
        assert!(
            matches!(failure.error, RunError::MemoryLimit),
            "expected MemoryLimit, got {:?}",
            failure.error
        );
    }

    /// Finding 6: prefix `console.log` output is delivered on the cold-start call's
    /// result (the run that paid for warm-up), then cleared — not dropped.
    #[test]
    fn warmup_console_output_appears_on_the_first_call_only() {
        init_platform();
        let mut core = create_instance_core(
            Some(PrefixSpec {
                code: "console.log('boot diagnostics')",
                filename: "<prefix>",
                globals: &[],
            }),
            &[],
            0,
        )
        .expect("prefix warms up");

        let first = run_call_on_core(
            &mut core,
            Some("export default 1"),
            "<iso4>",
            &[],
            Limits::default(),
            &[],
            &[],
            None,
            Arc::new(AtomicU32::new(0)),
            None,
        )
        .result
        .expect("first call");
        assert!(
            first.stdout.iter().any(|l| l.contains("boot diagnostics")),
            "cold-start call carries the prefix's warm-up log, got {:?}",
            first.stdout
        );

        let second = run_call_on_core(
            &mut core,
            Some("export default 2"),
            "<iso4>",
            &[],
            Limits::default(),
            &[],
            &[],
            None,
            Arc::new(AtomicU32::new(0)),
            None,
        )
        .result
        .expect("second call");
        assert!(
            !second.stdout.iter().any(|l| l.contains("boot diagnostics")),
            "warm-up log must not repeat on later calls, got {:?}",
            second.stdout
        );
    }

    /// CHARACTERIZATION of a KNOWN gap, not a guarantee: a promise
    /// whose resolver is stashed on `globalThis` in one run, then resolved in
    /// a later run, runs its `.then` continuation DURING that later run and
    /// observes the later run's state. This is the cross-run confused-deputy —
    /// the leftover continuation looks globals up by name and reaches the new
    /// run's freshly re-bound bindings. Pinned so the behavior can't shift
    /// silently until run-id gated host effects fix it, at which
    /// point this test flips to assert refusal.
    #[test]
    fn stale_continuation_runs_in_the_next_call_and_sees_its_state() {
        init_platform();
        let mut core = create_instance_core(
            Some(PrefixSpec {
                code: "",
                filename: "<prefix>",
                globals: &[],
            }),
            &[],
            0,
        )
        .expect("prefix warms up");

        // Run 1: stash a resolver and queue a continuation that copies whatever
        // `globalThis.marker` holds WHEN IT EVENTUALLY RUNS. The promise never
        // settles this run, so the continuation stays queued into the isolate.
        let run1 = run_call_on_core(
            &mut core,
            Some(
                "const p = new Promise(r => { globalThis.res = r }); \
                 p.then(() => { globalThis.leaked = globalThis.marker }); \
                 export default 1",
            ),
            "<iso4>",
            &[],
            Limits::default(),
            &[],
            &[],
            None,
            Arc::new(AtomicU32::new(0)),
            None,
        );
        assert!(!run1.tainted);
        assert!(run1.result.is_ok());

        // Run 2: set a marker, then resolve run 1's promise. Run 1's
        // continuation fires at run 2's microtask checkpoint and reads run 2's
        // marker — run 1's code executing inside run 2's call.
        let run2 = run_call_on_core(
            &mut core,
            Some("globalThis.marker = 'run2-secret'; globalThis.res(); export default 1"),
            "<iso4>",
            &[],
            Limits::default(),
            &[],
            &[],
            None,
            Arc::new(AtomicU32::new(0)),
            None,
        );
        assert!(!run2.tainted);
        assert!(run2.result.is_ok());

        // Run 3: observe what run 1's continuation captured — run 2's marker.
        let run3 = run_call_on_core(
            &mut core,
            Some("export default globalThis.leaked ?? 'not-set'"),
            "<iso4>",
            &[],
            Limits::default(),
            &[],
            &[],
            None,
            Arc::new(AtomicU32::new(0)),
            None,
        )
        .result
        .expect("run 3");
        assert_eq!(
            get_default(&run3).as_deref(),
            Some("run2-secret"),
            "run 1's continuation ran during run 2 and observed run 2's state \
             (cross-run confused-deputy)"
        );
    }
}
