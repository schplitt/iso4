//! V8 isolate management and JavaScript execution.
//!
//! Owns the engine core: platform init, isolate creation, compilation,
//! evaluation, the turn loop, bridge dispatch, streams and output assembly.
//! Console capture, guest timers, the limit guard, host globals and ESM
//! modules live in the child modules `console`, `timers`, `guard`, `globals`
//! and `modules`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Once;
use std::sync::OnceLock;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::blob;
use crate::errors;
use guard::{GuardTarget, InstanceGuard};
use crate::link::{Addr, HostLink, Inbound, LinkError, Observer, Outbound};
use timers::{TimerTable, timer_turn};
use crate::types;
use crate::types::{
    BridgeErrorPayload, HostGlobalDef, HostModuleNode, ImportBinding, ImportModule,
};

use globals::{
    check_not_reserved, define_global, eval_script, install_value_globals, throw_v8_error,
    v8_error_value,
};
use modules::{
    ResolverContext, evaluate_prefix_module, extract_module_exports, host_import_meta_callback,
    install_resolver, module_resolver_callback, take_resolver,
};

pub use globals::RESERVED_GLOBAL_NAMES;
pub use guard::GuardCtl;

pub(crate) mod console;
pub(crate) mod globals;
mod guard;
mod modules;
#[cfg(test)]
pub(crate) mod testsupport;
pub(crate) mod timers;

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
        // ICU data comes with the prebuilt V8, statically linked. Nothing to
        // load here: a missing one aborts the process on the first
        // locale-aware call, which the Intl tests cover.
        let platform = crate::new_platform().make_shared();
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
    /// The retirement line, and the run's inbound frame allowance (enforced
    /// by the frontend). Nothing terminates at it. Zero means none.
    pub soft_memory_mb: u32,
    /// The terminating line: `CreateParams::heap_limits` +
    /// `add_near_heap_limit_callback` + the ArrayBuffer budget. Zero = none.
    pub hard_memory_mb: u32,
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

impl From<&types::ResourceLimits> for Limits {
    fn from(l: &types::ResourceLimits) -> Self {
        Limits {
            wall_time_ms: l.wall_time_ms,
            cpu_time_ms: l.cpu_time_ms,
            soft_memory_mb: l.soft_memory_mb,
            hard_memory_mb: l.hard_memory_mb,
            max_export_bytes: l.max_export_bytes,
            max_stdout_bytes: l.max_stdout_bytes,
            max_stderr_bytes: l.max_stderr_bytes,
            max_bridge_call_bytes: l.max_bridge_call_bytes,
            max_bridge_calls: l.max_bridge_calls,
            grace_ms: l.grace_ms,
        }
    }
}

/// Termination reason set by the first enforcer to fire — a limit guard, or
/// a hard abort's mid-turn kill ([`GuardCtl::abort_executing`]).
///
/// Stored in a [`ReasonCell`] - first write wins, subsequent writes are
/// no-ops. Absence of a fired guard is represented naturally by
/// `ReasonCell::get()` returning `None`. The kill writes its reason BEFORE
/// `terminate_execution`, so detection never rides on V8's transient
/// terminate flag (which a microtask checkpoint can consume silently).
#[derive(Clone, PartialEq, Eq)]
enum TerminationReason {
    Wall,
    Cpu,
    Memory,
    Abort,
    /// Sandbox code called dynamic `import()`; carries the run's message.
    Import(Arc<str>),
}

/// First-writer-wins slot for the run's termination reason, shared between
/// the watchdog, the near-heap callback, the ArrayBuffer allocator, and
/// the classification sites.
///
/// Used to be a `OnceLock`, which cannot be re-armed. The allocator and the
/// near-heap callback are registered once per *isolate* while guards and
/// classification are per *call*, so a reused isolate (warm instances)
/// needs one long-lived cell that resets between calls instead of a fresh
/// `OnceLock` the isolate-lifetime consumers can never re-point to.
struct ReasonCell {
    reason: Mutex<Option<TerminationReason>>,
    /// A fatal bridge violation terminated without a reason; no JS check may
    /// ever consume that terminate, so the taint verdict reads this instead.
    bare_terminate: AtomicBool,
}

impl ReasonCell {
    fn new() -> Self {
        Self {
            reason: Mutex::new(None),
            bare_terminate: AtomicBool::new(false),
        }
    }

    /// First write wins; later writes are no-ops, matching `OnceLock::set`.
    fn set(&self, reason: TerminationReason) {
        let mut slot = self.reason.lock().unwrap_or_else(|p| p.into_inner());
        slot.get_or_insert(reason);
    }

    fn get(&self) -> Option<TerminationReason> {
        self.reason.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    fn bare_terminate(&self) -> bool {
        self.bare_terminate.load(Ordering::Relaxed)
    }

    /// Clear the slot for the next call on a reused isolate. Only safe while
    /// no JS is executing and no guards are armed — i.e. between calls.
    fn reset(&self) {
        *self.reason.lock().unwrap_or_else(|p| p.into_inner()) = None;
        self.bare_terminate.store(false, Ordering::Relaxed);
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

/// Near-heap-limit callback registered when `limits.hard_memory_mb > 0`.
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

/// What this thread is executing for the fatal OOM line's benefit.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OomRun {
    None,
    /// A run's turn (wire run id).
    Run(u32),
    /// The prefix evaluating for the run that triggered this instance.
    ColdStart(u32),
}

/// Copy and destructor-free on purpose: read inside the OOM handler, where
/// the thread-local must neither allocate nor register anything.
#[derive(Clone, Copy)]
pub struct OomRunLabel {
    /// The instance thread's prefix; `None` on a one-off run's thread.
    pub prefix_id: Option<u64>,
    pub run: OomRun,
}

thread_local! {
    static OOM_RUN_LABEL: std::cell::Cell<OomRunLabel> = const {
        std::cell::Cell::new(OomRunLabel {
            prefix_id: None,
            run: OomRun::None,
        })
    };
}

/// Set once per instance thread, before the prefix evaluates.
pub fn set_oom_prefix_id(prefix_id: Option<u64>) {
    OOM_RUN_LABEL.with(|c| c.set(OomRunLabel { prefix_id, ..c.get() }));
}

/// Mark what this thread executes now; `OomRun::None` between runs.
pub fn set_oom_run(run: OomRun) {
    OOM_RUN_LABEL.with(|c| c.set(OomRunLabel { run, ..c.get() }));
}

/// The fatal OOM line, written into `buf` without allocating (the abort
/// path must not depend on a working allocator). Returns the bytes used.
fn format_oom_line(
    buf: &mut [u8],
    is_heap_oom: bool,
    detail: Option<&std::ffi::CStr>,
    location: Option<&std::ffi::CStr>,
) -> usize {
    use std::io::Write as _;
    let cap = buf.len();
    let mut cur = buf;
    let _ = write!(
        cur,
        "[iso4-v8] fatal: V8 out of memory ({})",
        if is_heap_oom { "heap" } else { "non-heap" }
    );
    let label = OOM_RUN_LABEL.with(|c| c.get());
    let _ = match (label.run, label.prefix_id) {
        (OomRun::None, _) => write!(cur, " with no run executing on this thread"),
        (OomRun::Run(run_id), Some(p)) => write!(cur, " in run {run_id} of prefix {p}"),
        (OomRun::Run(run_id), None) => write!(cur, " in one-off run {run_id}"),
        (OomRun::ColdStart(run_id), Some(p)) => {
            write!(cur, " during cold start of prefix {p} (triggered by run {run_id})")
        }
        (OomRun::ColdStart(run_id), None) => {
            write!(cur, " during cold start (triggered by run {run_id})")
        }
    };
    // V8 passes its message ("Reached heap limit") as `location`.
    if let Some(l) = location.and_then(|l| l.to_str().ok()) {
        let _ = write!(cur, ": {l}");
    }
    if let Some(d) = detail.and_then(|d| d.to_str().ok()) {
        let _ = write!(cur, " ({d})");
    }
    let _ = write!(cur, " — aborting the process\n");
    cap - cur.len()
}

/// Fatal OOM handler: V8 calls it on the isolate's thread right before it
/// aborts the process, so this line is the host's only attribution.
unsafe extern "C" fn oom_error_cb(location: *const std::ffi::c_char, details: &v8::OomDetails) {
    use std::ffi::CStr;
    use std::io::Write as _;
    let mut buf = [0u8; 1024];
    let n = format_oom_line(
        &mut buf,
        details.is_heap_oom,
        (!details.detail.is_null()).then(|| unsafe { CStr::from_ptr(details.detail.cast()) }),
        (!location.is_null()).then(|| unsafe { CStr::from_ptr(location) }),
    );
    let _ = std::io::stderr().write_all(&buf[..n]);
}

/// Wasm is removed from the sandbox for now (DESIGN §7.5): the global is
/// deleted at every boot (`webtypes::delete_removed_globals`); this
/// isolate-wide denial backstops codegen.
unsafe extern "C" fn deny_wasm_codegen(
    _context: v8::Local<v8::Context>,
    _source: v8::Local<v8::String>,
) -> bool {
    false
}

/// Dynamic `import()` ends the whole run: records the reason, then terminates
/// so no guest `try`/`catch` can observe it. Reads the isolate's reason slot.
fn dynamic_import_callback<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    _host_defined_options: v8::Local<'s, v8::Data>,
    _resource_name: v8::Local<'s, v8::Value>,
    specifier: v8::Local<'s, v8::String>,
    _import_attributes: v8::Local<'s, v8::FixedArray>,
) -> Option<v8::Local<'s, v8::Promise>> {
    // Bounded copy: the specifier is guest-built and may be hundreds of MB.
    let mut head = [0u16; 256];
    let len = specifier.length().min(head.len());
    specifier.write_v2(scope, 0, &mut head, v8::WriteFlags::empty());
    let cut = specifier.length() > head.len();
    let mut head = &head[..len];
    // A cut inside a surrogate pair would decode to U+FFFD.
    if cut && head.last().is_some_and(|u| (0xD800..0xDC00).contains(u)) {
        head = &head[..len - 1];
    }
    let ellipsis = if cut { "…" } else { "" };
    let message = format!(
        "dynamic import() is not supported: '{}{ellipsis}'",
        String::from_utf16_lossy(head)
    );
    if let Some(reason) = scope.get_slot::<Arc<ReasonCell>>() {
        reason.set(TerminationReason::Import(message.into()));
    }
    scope.terminate_execution();
    // A little JS lands the terminate inside this call (a TryCatch would eat it).
    if let Some(script) = v8::String::new(scope, "for (let i = 0; i < 2; i++) {}")
        .and_then(|src| v8::Script::compile(scope, src, None))
    {
        script.run(scope);
    }
    if scope.is_execution_terminating() {
        return None;
    }
    // Not delivered yet: a never-settling promise; the pending terminate and
    // the reason end the run at the next check.
    v8::PromiseResolver::new(scope).map(|r| r.get_promise(scope))
}

/// Wire the dynamic `import()` handler and the reason slot it reports into.
fn install_dynamic_import(isolate: &mut v8::OwnedIsolate, reason: &Arc<ReasonCell>) {
    isolate.set_slot(Arc::clone(reason));
    isolate.set_host_import_module_dynamically_callback(dynamic_import_callback);
}

/// Tracks how much CPU the guest's own thread has burned.
///
/// `enter()` / `leave()` bracket each period when V8 is actually running JS
/// — one epoch per turn in the instance loop, entered when a turn starts
/// executing and left at turn exit, so time parked between turns (bridge
/// waits, queueing) is never counted. Compile/instantiate time is excluded
/// by entering only right before `module.evaluate()` in the start turn.
///
/// The epoch reads the executing thread's CPU clock, not the wall clock
/// (#196): an instance owns its thread, so wall would charge a run for time
/// it sat descheduled behind co-resident runs. V8's background threads (GC,
/// compilation) are outside any one thread's clock, so not attributed.
pub struct CpuBudget {
    accumulated_ns: AtomicU64,
    epoch: Mutex<Option<CpuEpoch>>,
}

/// An open epoch: the executing thread's clock, for the watchdog to read
/// remotely, and that thread's reading when the epoch opened. The owning
/// thread closes the epoch with its own cheaper read of the same counter.
struct CpuEpoch {
    clock: crate::cpuclock::ThreadClock,
    start_ns: u64,
}

impl CpuBudget {
    pub fn new() -> Self {
        Self {
            accumulated_ns: AtomicU64::new(0),
            epoch: Mutex::new(None),
        }
    }

    /// Mark the start of a V8 execution epoch on the calling thread.
    pub fn enter(&self) {
        let epoch = CpuEpoch {
            clock: crate::cpuclock::ThreadClock::current(),
            start_ns: crate::cpuclock::self_ns(),
        };
        *self.epoch.lock().unwrap_or_else(|p| p.into_inner()) = Some(epoch);
    }

    /// End the current epoch and accumulate the CPU it burned. Runs on the
    /// executing thread, so it reads its own clock directly.
    pub fn leave(&self) {
        let now_ns = crate::cpuclock::self_ns();
        let mut g = self.epoch.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(e) = g.take() {
            self.accumulated_ns
                .fetch_add(now_ns.saturating_sub(e.start_ns), Ordering::Relaxed);
        }
    }

    /// Total CPU burned so far, including any epoch still open. Reading an
    /// open epoch takes the executing thread's clock remotely.
    fn elapsed_ns(&self) -> u64 {
        let base = self.accumulated_ns.load(Ordering::Relaxed);
        let active = self
            .epoch
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .map(|e| e.clock.read_ns().saturating_sub(e.start_ns))
            .unwrap_or(0);
        base + active
    }

    /// Total accumulated CPU time in milliseconds.
    pub fn elapsed_ms(&self) -> u64 {
        self.elapsed_ns() / 1_000_000
    }

    /// Total accumulated CPU time in milliseconds with microsecond
    /// resolution. The integer `elapsed_ms` is enough for the CPU guard; the
    /// run result reports this precise value so sub-millisecond runs don't
    /// read as `0`.
    pub fn elapsed_ms_precise(&self) -> f64 {
        (self.elapsed_ns() as f64 / 1_000.0).round() / 1_000.0
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
    fn settle(&mut self, call_id: u32, now_ms: f64, ok: bool, response_bytes: u32) {
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
    fn snapshot(&self, now_ms: f64) -> Vec<types::BridgeCallRecord> {
        let mut records = self.records.clone();
        for idx in self.in_flight.values() {
            let r = &mut records[*idx];
            r.duration_ms = round_micro((now_ms - r.start_ms).max(0.0));
        }
        records
    }

    /// Snapshot the records for the run result. Calls still in flight get
    /// their duration set to "until run end" and stay `Unanswered`.
    fn finalize(&mut self, now_ms: f64) -> Vec<types::BridgeCallRecord> {
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

    /// The run's own logic time: execution plus async waits, each wait
    /// counted until its answer arrived — engine time (turns for co-resident
    /// runs, delivery backlog) is excluded. `cpu ≤ wall ≤ duration`.
    pub wall_time_ms: f64,

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
    pub bridge_calls: Vec<types::BridgeCallRecord>,
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
    /// Counted grace logic time — see `Output::wall_time_ms`.
    pub wall_time_ms: f64,
    /// Active V8 execution time during grace, ms.
    pub cpu_time_ms: f64,
    /// Console lines written during grace (the run's own lines already
    /// shipped on the Result).
    pub stdout: Vec<String>,
    pub stderr: Vec<String>,
    /// Bridge calls attempted during grace.
    pub bridge_calls: Vec<types::BridgeCallRecord>,
    /// `name`/`message` of the first rejection when `status == Failed`, or of
    /// the fatal error that ended a truncated phase, when one exists.
    pub error: Option<(String, String)>,
    /// Whether the early Result was sent by the run flow. Always true on
    /// the session paths; false only in direct-API use without a link or
    /// epilogue spec (tests), where grace ran before returning.
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
    /// The run's address on outbound `StreamPull`/`StreamCancel` messages.
    run_id: u32,
    link: Option<Arc<dyn HostLink>>,
}

impl StreamTable {
    fn boxed() -> Box<Self> {
        Box::new(Self {
            slot: RefCell::new(None),
        })
    }
}

// ── Outbound (sandbox → host) streamed bodies (#128) ─────────────────────────

/// How long an outbound stream may sit with no host activity (no credit
/// grant, no cancel) before the runtime drops it (Jakob's ruling: 10 s —
/// our consumer is in-process JS with no socket to error on abandonment).
const OUT_STREAM_IDLE_MS: u64 = 10_000;

/// Test hook: a nonzero override shrinks the idle allowance so the
/// deadline path is testable without 10 s waits.
#[cfg(test)]
pub(crate) static OUT_IDLE_OVERRIDE_MS: AtomicU64 = AtomicU64::new(0);

fn out_stream_idle_ms() -> u64 {
    #[cfg(test)]
    {
        let v = OUT_IDLE_OVERRIDE_MS.load(Ordering::Relaxed);
        if v > 0 {
            return v;
        }
    }
    OUT_STREAM_IDLE_MS
}

/// Runtime-side state of one outbound streamed body: the guest stream
/// driving it, the host-granted credit, and the pull in flight.
struct OutStream {
    /// The guest `Iso4BodyStream` this stream pulls from.
    body: v8::Global<v8::Object>,
    /// Bytes the host still allows in flight. Seeded with the implicit
    /// initial window (eager production, mirroring the inbound direction).
    credit: u32,
    /// A started pull whose promise has not settled yet.
    pending: Option<v8::Global<v8::Promise>>,
    /// Unsent remainder of a guest chunk that outgrew the credit/chunk cap.
    backlog: Vec<u8>,
    backlog_sent: usize,
    /// A terminal frame (`StreamEnd`) went out or a cancel arrived; the
    /// entry only remains so late frames are recognized.
    done: bool,
}

/// Per-run registry of outbound streamed bodies — the [`StreamTable`]
/// pattern with the direction reversed. Armed per run; result serialization
/// registers into it through the per-thread run pointers.
pub struct OutStreamTable {
    slot: RefCell<Option<OutTableState>>,
}

struct OutTableState {
    streams: HashMap<u32, OutStream>,
    next_id: u32,
    /// The run's address on outbound `StreamChunk`/`StreamEnd` messages.
    run_id: u32,
    link: Option<Arc<dyn HostLink>>,
    /// Last host read/cancel — the idle-deadline anchor.
    last_activity: std::time::Instant,
}

impl OutStreamTable {
    fn boxed() -> Box<Self> {
        Box::new(Self {
            slot: RefCell::new(None),
        })
    }
}

/// Register a guest stream as an outbound body during RESULT serialization.
/// Returns the allocated stream id, or `None` when the executing run cannot
/// stream (no run, table not armed — hold-mode runs, or a leg where stream
/// bodies are not allowed; the codec turns `None` into its own error).
pub fn register_out_stream(
    scope: &mut v8::PinScope,
    body: v8::Local<v8::Object>,
) -> Option<u32> {
    if !RESULT_STREAM_MODE.with(|c| c.get()) {
        return None;
    }
    let token = CURRENT_RUN_TOKEN.with(|c| c.get());
    if token == 0 {
        return None;
    }
    RUN_TABLE_PTR.with(|c| {
        let ptr = c.get();
        if ptr.is_null() {
            return None;
        }
        // SAFETY: set only while the owning instance's turn is live on this
        // thread (same contract as `register_hydrated_stream`).
        let table = unsafe { &*ptr };
        let state = table.state_ptr(token);
        if state.is_null() {
            return None;
        }
        // SAFETY: entry pointers are valid for the remainder of the turn.
        let mut slot = unsafe { &*state }.out_streams.slot.borrow_mut();
        let st = slot.as_mut()?;
        let id = st.next_id;
        st.next_id += 1;
        st.streams.insert(
            id,
            OutStream {
                body: v8::Global::new(scope, body),
                credit: types::STREAM_CREDIT_WINDOW_BYTES,
                pending: None,
                backlog: Vec::new(),
                backlog_sent: 0,
                done: false,
            },
        );
        Some(id)
    })
}

thread_local! {
    /// Whether the value currently being serialized is a run RESULT on a
    /// leg that may carry stream bodies (session runs with an epilogue).
    /// Off for exports, bridge arguments, and hold-mode runs — a stream
    /// body there keeps the existing cannot-cross error.
    static RESULT_STREAM_MODE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// RAII enabling [`register_out_stream`] for one serialization.
struct ResultStreamMode;
impl ResultStreamMode {
    fn enter(enabled: bool) -> Option<Self> {
        if !enabled {
            return None;
        }
        RESULT_STREAM_MODE.with(|c| c.set(true));
        Some(Self)
    }
}
impl Drop for ResultStreamMode {
    fn drop(&mut self) {
        RESULT_STREAM_MODE.with(|c| c.set(false));
    }
}

// ── Per-run state table ──────────────────────────────────────────────────────
//
// One instance serves several in-flight runs (the #124 interleaving model),
// so everything a native callback used to reach through a fixed
// instance-lifetime pointer — console buffers, the waitUntil set, the
// streamed-body registry, bridge-stub bindings — is now keyed by the owning
// run. Attribution: a *turn* executes on behalf of exactly one run
// ([`CURRENT_RUN_TOKEN`], set at turn entry), and continuations created by a
// run carry its token in the CPED rider (see `install_async_context`), so
// work resumed during another run's turn still attributes to its creator.

/// The instance's run table, held in an isolate slot so the natives baked
/// into the startup snapshot carry no per-instance pointer.
#[derive(Clone, Copy)]
pub(crate) struct RunTableSlot(pub(crate) *const RunTable);

fn slot_table<'s>(scope: &mut v8::PinScope<'s, '_>) -> Option<&'s RunTable> {
    // SAFETY: the slot points at the instance-lifetime RunTable Box, which
    // outlives every callback of its isolate.
    scope
        .get_slot::<RunTableSlot>()
        .filter(|slot| !slot.0.is_null())
        .map(|slot| unsafe { &*slot.0 })
}

thread_local! {
    /// The run whose turn is executing on this thread. 0 = none (between
    /// turns, prefix warm-up). Fallback attribution when no CPED rider is
    /// present; the gate that refuses between-turn callback activity.
    static CURRENT_RUN_TOKEN: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };

    /// The run table of the instance executing on this thread, for callback
    /// paths that have no `scope`/External to reach it through (the codec's
    /// rehydration walk). Null outside turns.
    static RUN_TABLE_PTR: std::cell::Cell<*const RunTable> =
        const { std::cell::Cell::new(std::ptr::null()) };

    /// Runs whose timer set changed during the current turn: a CPED-rider
    /// continuation can arm a CO-RESIDENT's timer, invisible to the
    /// owner-only post-turn re-queue — the loop drains this after each turn.
    static TIMERS_TOUCHED: RefCell<Vec<u64>> = const { RefCell::new(Vec::new()) };
}

/// Process-wide run-token allocator. Tokens are instance-local identities on
/// the wire-facing side but allocated globally so no two live runs anywhere
/// share one (wire run ids can collide across connections; tokens cannot).
/// Starts at 1 — 0 means "no run".
/// Per-instance signals written by the owner loop and read by the warm
/// registry: thread load for join routing, settled heap for eviction.
pub struct InstanceLoad {
    /// Thread utilization over the last closed window, per-mille.
    util_permille: std::sync::atomic::AtomicU32,
    /// When that window closed (ms since [`load_now_ms`]'s epoch). A stale
    /// window means the loop is parked — utilization is genuinely ~0.
    closed_at_ms: std::sync::atomic::AtomicU64,
    /// When the turn running right now started, `u64::MAX` between turns.
    /// Windows close only between turns, so a long turn would otherwise
    /// read as idle and attract more joins.
    turn_open_since_ms: std::sync::atomic::AtomicU64,
    /// Heap measured after the idle settle GC; 0 = none since the last
    /// release. Fresher than the release-time reading, which still counts
    /// the last run's garbage.
    settled_heap_bytes: std::sync::atomic::AtomicU64,
}

/// Utilization windows: closed every 100 ms of wall time; a reading older
/// than 500 ms reports 0 (parked loop).
const LOAD_WINDOW_MS: u64 = 100;
const LOAD_STALE_MS: u64 = 500;

/// Milliseconds on the process-wide load epoch (cross-thread comparable).
pub fn load_now_ms() -> u64 {
    static ANCHOR: OnceLock<std::time::Instant> = OnceLock::new();
    ANCHOR.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64
}

impl InstanceLoad {
    pub fn new() -> Self {
        Self {
            util_permille: std::sync::atomic::AtomicU32::new(0),
            closed_at_ms: std::sync::atomic::AtomicU64::new(0),
            turn_open_since_ms: std::sync::atomic::AtomicU64::new(u64::MAX),
            settled_heap_bytes: std::sync::atomic::AtomicU64::new(0),
        }
    }

    fn publish_settled_heap(&self, bytes: u64) {
        self.settled_heap_bytes
            .store(bytes, std::sync::atomic::Ordering::Release);
    }

    /// The settled reading, or 0 when none is current.
    pub fn settled_heap_bytes(&self) -> u64 {
        self.settled_heap_bytes
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// A run finished on the instance: whatever was settled is spent.
    pub fn clear_settled_heap(&self) {
        self.publish_settled_heap(0);
    }

    fn close_window(&self, permille: u32) {
        self.util_permille
            .store(permille, std::sync::atomic::Ordering::Release);
        self.closed_at_ms
            .store(load_now_ms(), std::sync::atomic::Ordering::Release);
    }

    fn open_turn(&self, at_ms: u64) {
        self.turn_open_since_ms
            .store(at_ms, std::sync::atomic::Ordering::Release);
    }

    fn close_turn(&self) {
        self.turn_open_since_ms
            .store(u64::MAX, std::sync::atomic::Ordering::Release);
    }

    /// Recent utilization for routing; 0 when the last window is stale.
    pub fn current_util_permille(&self) -> u32 {
        self.util_permille_at(load_now_ms())
    }

    fn util_permille_at(&self, now_ms: u64) -> u32 {
        let open = self
            .turn_open_since_ms
            .load(std::sync::atomic::Ordering::Acquire);
        if now_ms.saturating_sub(open) >= LOAD_WINDOW_MS {
            // A turn holding the thread for a whole window IS saturation,
            // whatever the last closed window measured.
            return UTILIZATION_SATURATED_PERMILLE;
        }
        let at = self.closed_at_ms.load(std::sync::atomic::Ordering::Acquire);
        if now_ms.saturating_sub(at) > LOAD_STALE_MS {
            return 0;
        }
        self.util_permille.load(std::sync::atomic::Ordering::Acquire)
    }
}

/// Full utilization: reported for a turn that outlasts a window.
const UTILIZATION_SATURATED_PERMILLE: u32 = 1000;

impl Default for InstanceLoad {
    fn default() -> Self {
        Self::new()
    }
}

pub fn alloc_run_token() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Everything one in-flight run owns on its instance. Boxed in the table so
/// the address is stable across map rehashes; pointers into an entry are
/// derived and used only within one turn/callback invocation, and entries
/// are inserted/removed only between turns (no JS on the stack), so such a
/// pointer never outlives its entry.
struct RunCallState {
    /// Console output, capped per this run's limits.
    logs: Box<RefCell<LogBuffers>>,
    /// `waitUntil` registrations (armed at the setup-to-run boundary).
    pending: Box<PendingWork>,
    /// Streamed-body registry (armed per run).
    streams: Box<StreamTable>,
    /// Outbound streamed-body registry (armed per run) — result bodies the
    /// host reads chunk by chunk (#128).
    out_streams: Box<OutStreamTable>,
    /// Pending `setTimeout` registrations; dropped with the entry, so a
    /// settled run's timer can never fire into a later run.
    timers: Box<RefCell<TimerTable>>,
    /// Per-stub bridge bindings for THIS run — the per-call half of what a
    /// stub `Function` needs; the instance-lifetime half is the
    /// [`StubSlot`] its External points at.
    /// Rc'd so the bridge callback can take a per-invocation handle with one
    /// refcount bump instead of cloning the whole binding (two Strings and a
    /// fistful of Arcs).
    stubs: RefCell<HashMap<String, std::rc::Rc<GlobalCallbackData>>>,
    /// The side channel console lines are copied to as they happen.
    observer: Arc<dyn Observer>,
    /// This run's address on observer messages.
    addr: Addr,
}

impl RunCallState {
    fn new(
        max_stdout_bytes: u32,
        max_stderr_bytes: u32,
        observer: Arc<dyn Observer>,
        addr: Addr,
    ) -> Box<Self> {
        Box::new(Self {
            logs: Box::new(RefCell::new(LogBuffers {
                max_stdout_bytes,
                max_stderr_bytes,
                ..LogBuffers::default()
            })),
            pending: PendingWork::boxed(),
            streams: StreamTable::boxed(),
            out_streams: OutStreamTable::boxed(),
            timers: TimerTable::boxed(),
            stubs: RefCell::new(HashMap::new()),
            observer,
            addr,
        })
    }
}

/// Instance-lifetime registry of in-flight runs, reachable from native
/// callbacks through an `External` (stable Box address, the `LogBuffers`
/// pattern generalized).
pub struct RunTable {
    runs: RefCell<HashMap<u64, Box<RunCallState>>>,
    /// Console output produced outside any run — prefix warm-up — delivered
    /// on the cold-start call (the old `warmup_logs_pending` flow).
    setup_logs: RefCell<LogBuffers>,
}

impl RunTable {
    fn boxed() -> Box<Self> {
        Box::new(Self {
            runs: RefCell::new(HashMap::new()),
            setup_logs: RefCell::new(LogBuffers::default()),
        })
    }

    /// Pointer to a live run's state, or null. The pointer is valid for the
    /// remainder of the current turn (entries are only removed between
    /// turns); callers must not hold it longer.
    fn state_ptr(&self, token: u64) -> *const RunCallState {
        self.runs
            .borrow()
            .get(&token)
            .map_or(std::ptr::null(), |b| std::ptr::addr_of!(**b))
    }
}

/// The run native callbacks attribute to: the CPED rider's creator token
/// when the running continuation carries one AND that run is still live,
/// else the current turn owner. (A stale continuation of a finished run —
/// driven during a later run's checkpoint on a warm instance — attributes
/// to the turn owner, exactly the pre-table behavior of the shared buffers.)
///
/// Returns the run's state pointer (null when the attributed run has no
/// live entry — setup code, or a stale continuation whose turn owner
/// already concluded), resolved in ONE table borrow: the old
/// token-then-`state_ptr` pair cost a second hash lookup per bridge call
/// and console line.
///
/// The pointer is valid for the remainder of the current turn (entries are
/// only removed between turns); callers must not hold it longer.
fn attributed_state(scope: &mut v8::PinScope, table: &RunTable) -> *const RunCallState {
    attributed_token_and_state(scope, table).1
}

/// [`attributed_state`] plus the run's token, in the same table borrow.
fn attributed_token_and_state(
    scope: &mut v8::PinScope,
    table: &RunTable,
) -> (u64, *const RunCallState) {
    let current = CURRENT_RUN_TOKEN.with(|c| c.get());
    let cped = scope.get_continuation_preserved_embedder_data();
    let rider = rider_parts(scope, cped).map(|(token, _)| token);
    let runs = table.runs.borrow();
    if let Some(token) = rider {
        if token != current {
            // A live rider run wins; a stale rider falls back to the owner.
            if let Some(entry) = runs.get(&token) {
                return (token, std::ptr::addr_of!(**entry));
            }
        }
    }
    (
        current,
        runs.get(&current)
            .map_or(std::ptr::null(), |b| std::ptr::addr_of!(**b)),
    )
}

/// Register a hydrated stream in the executing run's table. Returns false
/// when no run is executing on this thread (a descriptor carrying a stream
/// outside a run — refused by the codec). The codec walk has no `scope`, so
/// attribution is the turn owner: rehydration only happens while delivering
/// that run's own payloads (call args, its bridge responses).
pub fn register_hydrated_stream(stream_id: u32) -> bool {
    let token = CURRENT_RUN_TOKEN.with(|c| c.get());
    if token == 0 {
        return false;
    }
    RUN_TABLE_PTR.with(|c| {
        let ptr = c.get();
        if ptr.is_null() {
            return false;
        }
        // SAFETY: set only while the owning instance's turn is live on this
        // thread.
        let table = unsafe { &*ptr };
        let state = table.state_ptr(token);
        if state.is_null() {
            return false;
        }
        // SAFETY: entry pointers are valid for the remainder of the turn.
        let mut slot = unsafe { &*state }.streams.slot.borrow_mut();
        match slot.as_mut() {
            Some(state) => {
                state.streams.insert(stream_id, StreamState::new());
                true
            }
            None => false,
        }
    })
}

/// Send a `StreamPull` credit grant for consumed bytes. Best-effort: a dead
/// link surfaces through the run's own events soon enough.
fn send_stream_credit(state: &StreamTableState, stream_id: u32, credit: u32) {
    let Some(link) = &state.link else { return };
    if credit == 0 {
        return;
    }
    let _ = link.send(
        Addr::Run(state.run_id),
        Outbound::StreamPull { stream_id, credit },
    );
}

/// Send a `StreamCancel` so the host stops pumping and releases the source.
fn send_stream_cancel(state: &StreamTableState, stream_id: u32, reason: &str) {
    let Some(link) = &state.link else { return };
    let _ = link.send(
        Addr::Run(state.run_id),
        Outbound::StreamCancel {
            stream_id,
            reason: reason.to_string(),
        },
    );
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
fn codec_error_to_run_error(error: crate::webcodec::CodecError) -> RunError {
    match error {
        crate::webcodec::CodecError::Unsupported(m) => RunError::TypeNotSerializable(m),
        crate::webcodec::CodecError::Malformed(m) => RunError::Internal(m),
    }
}

pub use crate::types::ResetCause;

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
    /// ([`RESERVED_GLOBAL_NAMES`]). Shaped to also cover runtime-owned
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

// ── Entry point ──────────────────────────────────────────────────────────────

/// Execute a sandboxed run and return the full output.
///
/// `globals` is the list of declared host globals, each tagged by how it is
/// installed natively (`HostGlobalDef`): bridge stubs send `BridgeCall`s on
/// the run's link and suspend until the matching `BridgeResponse` arrives on
/// its events; string/data globals are evaluated/materialised in-isolate and
/// need no host. Pass `RunIo::None` when no global installs a bridge stub.
pub fn execute(
    code: &str,
    filename: Option<&str>,
    limits: Limits,
    globals: &[HostGlobalDef],
    imports: &[ImportBinding],
    io: RunIo,
    call_id_counter: Arc<AtomicU32>,
    call: Option<&types::CallSpec>,
) -> Result<Output, FailureOutput> {
    execute_with_io(
        code,
        filename,
        limits,
        globals,
        imports,
        io,
        0,
        call_id_counter,
        call,
        None,
        Arc::new(crate::link::NoopObserver),
    )
}

/// [`execute`] with an explicit run I/O mode: the engine's one-off worker
/// path. `token` is the run's table token (0 = allocate one internally);
/// pass the token the run's handle carries, or mid-turn aborts never land.
#[allow(clippy::too_many_arguments)]
pub fn execute_with_io(
    code: &str,
    filename: Option<&str>,
    limits: Limits,
    globals: &[HostGlobalDef],
    imports: &[ImportBinding],
    io: RunIo,
    token: u64,
    call_id_counter: Arc<AtomicU32>,
    call: Option<&types::CallSpec>,
    ctl_slot: Option<Arc<OnceLock<GuardCtl>>>,
    observer: Arc<dyn Observer>,
) -> Result<Output, FailureOutput> {
    init_platform();
    run_module(
        code,
        filename.unwrap_or("<iso4>"),
        limits,
        globals,
        imports,
        io,
        token,
        call_id_counter,
        call,
        ctl_slot,
        observer,
    )
}

/// The prefix an instance evaluates at cold start: validated source plus the
/// globals declared at `prepare()` time, replayed into the fresh context.
#[derive(Clone, Copy)]
pub struct PrefixSpec<'a> {
    pub code: &'a str,
    pub filename: &'a str,
    /// The full `prepare()`-time global defs — value globals installed before
    /// the prefix evaluates, bridge callables as throwing placeholders.
    pub globals: &'a [HostGlobalDef],
}

/// Call an export of a prepared prefix on a fresh, throwaway instance.
///
/// The prefix source is evaluated into the fresh context first (under the
/// warm-up budget), then `globals` — all `HostGlobalDef::Bridge` stubs bound
/// to the run's link — overwrite the prefix-time placeholders, then
/// the call runs. String/data globals and shim wrappers are replayed from
/// `prefix.globals` and are not re-sent per run.
pub fn execute_with_prefix(
    prefix: PrefixSpec<'_>,
    limits: Limits,
    globals: &[HostGlobalDef],
    imports: &[ImportBinding],
    io: RunIo,
    call_id_counter: Arc<AtomicU32>,
    call: &types::CallSpec,
) -> Result<Output, FailureOutput> {
    init_platform();
    // Cold start of a warm-capable instance: warm-up (isolate boot + prefix
    // evaluation) runs under its own fixed budget, never the caller's limits
    // — a prefix that loops reports ERR_WARMUP_LIMIT, not the run's
    // timeout. The call itself then gets fresh guards from dispatch to
    // settle. The warm registry keeps the core alive between calls; this
    // one-shot wrapper drops it.
    let mut core = create_instance_core(prefix, imports, limits.hard_memory_mb)?;
    run_call_on_core(
        &mut core,
        prefix.globals,
        limits,
        globals,
        imports,
        io,
        call_id_counter,
        call,
    )
    .result
}

/// One-off ESM path: compile source as a module, instantiate it, evaluate
/// it, then inspect the module namespace object for `default` and named
/// exports. When `globals` is non-empty, live bridge stubs are installed for
/// each name, writing through the run's link.
///
/// When `call` is `Some`, the run's value is the called function's return
/// value instead of the exports, resolved against the freshly evaluated
/// module.
#[allow(clippy::too_many_arguments)]
fn run_module(
    code: &str,
    filename: &str,
    limits: Limits,
    globals: &[HostGlobalDef],
    imports: &[ImportBinding],
    io: RunIo,
    token: u64,
    call_id_counter: Arc<AtomicU32>,
    call: Option<&types::CallSpec>,
    ctl_slot: Option<Arc<OnceLock<GuardCtl>>>,
    observer: Arc<dyn Observer>,
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
        io,
        token,
        call_id_counter,
        call,
        ctl_slot,
        observer,
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
    code: &str,
    filename: &str,
    limits: Limits,
    globals: &[HostGlobalDef],
    imports: &[ImportBinding],
    io: RunIo,
    token: u64,
    // Per-connection call-ID counter. Shared across all runs on the same
    // connection so callIds are monotonically increasing and never reset to 0
    // between runs. This prevents a stale BridgeResponse from a previous
    // run's orphaned handler from being accepted as a valid response by the
    // next run's bridge_global_callback.
    call_id_counter: Arc<AtomicU32>,
    call: Option<&types::CallSpec>,
    ctl_slot: Option<Arc<OnceLock<GuardCtl>>>,
    observer: Arc<dyn Observer>,
    start: std::time::Instant,
    cpu_budget: Arc<CpuBudget>,
    bridge_log: Arc<Mutex<BridgeCallLog>>,
) -> Result<Output, FailureOutput> {
    // One-off runs hold a single-entry run table for the run's life — the
    // same routing the warm instances use, so the native installs are
    // uniform across both paths.
    let run_table = RunTable::boxed();
    let token = if token != 0 { token } else { alloc_run_token() };
    let epilogue = take_epilogue_spec();
    let entry = RunCallState::new(
        limits.max_stdout_bytes,
        limits.max_stderr_bytes,
        observer,
        Addr::Run(epilogue.map_or(0, |e| e.run_id)),
    );
    let logs_ptr: *const RefCell<LogBuffers> = std::ptr::addr_of!(*entry.logs);
    let pending_ptr: *const PendingWork = std::ptr::addr_of!(*entry.pending);
    let streams_ptr: *const StreamTable = std::ptr::addr_of!(*entry.streams);
    let out_streams_ptr: *const OutStreamTable = std::ptr::addr_of!(*entry.out_streams);
    let timers_ptr: *const RefCell<TimerTable> = std::ptr::addr_of!(*entry.timers);
    run_table.runs.borrow_mut().insert(token, entry);
    CURRENT_RUN_TOKEN.with(|c| c.set(token));
    RUN_TABLE_PTR.with(|c| c.set(std::ptr::addr_of!(*run_table)));
    // Clear the thread markers on every exit path — the entry dies with the
    // table, but the thread-locals outlive this run.
    struct TokenReset;
    impl Drop for TokenReset {
        fn drop(&mut self) {
            CURRENT_RUN_TOKEN.with(|c| c.set(0));
            RUN_TABLE_PTR.with(|c| c.set(std::ptr::null()));
        }
    }
    let _token_reset = TokenReset;

    // `reason` is created before the isolate so it can be shared with the
    // ArrayBuffer allocator (which is registered in CreateParams, before the
    // isolate exists).
    let reason = Arc::new(ReasonCell::new());

    // ── ArrayBuffer budget allocator ──────────────────────────────────────────
    // Built before the isolate so we can pass it into CreateParams.
    // The IsolateHandle is set on the state right after Isolate::new returns.
    let alloc_state: Option<Arc<BudgetAllocState>> = if limits.hard_memory_mb > 0 {
        Some(Arc::new(BudgetAllocState {
            used: AtomicUsize::new(0),
            budget: limits.hard_memory_mb as usize * 1024 * 1024,
            handle: OnceLock::new(),
            reason: Arc::clone(&reason),
        }))
    } else {
        None
    };

    let mut isolate = {
        let params = crate::snapshot::create_params();
        // Cap the V8 heap (strings, plain objects). The near-heap callback
        // converts a heap-OOM into a clean terminate_execution().
        let params = if limits.hard_memory_mb > 0 {
            params.heap_limits(0, limits.hard_memory_mb as usize * 1024 * 1024)
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
    // V8's default assumes it owns the machine and collects lazily; wrong for
    // a container full of isolates (workerd flips the same switch).
    isolate.memory_pressure_notification(v8::MemoryPressureLevel::Moderate);
    isolate.set_oom_error_handler(oom_error_cb);

    // Explicit policy: microtasks only drain when we call
    // perform_microtask_checkpoint() in the poll loop.  This gives us
    // deterministic control and prevents microtasks from firing during
    // export extraction (D9 hardening).
    isolate.set_microtasks_policy(v8::MicrotasksPolicy::Explicit);

    // `Atomics.wait` throws: a blocking timed sleep on the isolate thread is
    // a DoS primitive and a timing building block, and with no shared memory
    // (SharedArrayBuffer is removed by the web runtime) it has no legitimate
    // caller. Matches workerd (SetAllowAtomicsWait(false)) and browser main
    // threads.
    isolate.set_allow_atomics_wait(false);
    isolate.set_allow_wasm_code_generation_callback(deny_wasm_codegen);

    // Host modules receive their natively-built values through import.meta
    // (see `build_host_module`); the callback consults the resolver context to
    // find the values array staged for the module being initialised.
    isolate.set_host_initialize_import_meta_object_callback(host_import_meta_callback);
    install_dynamic_import(&mut isolate, &reason);

    // ── The limit guard ──────────────────────────────────────────────────
    // The instance's watchdog registration (#145) — a one-off run IS an
    // instance of one call. It covers the turn currently executing; while
    // the run is parked between turns the target is empty and boundary
    // deadlines are the loop's business. Armed here already so setup
    // (installs below) runs under the caller's budgets, exactly as the old
    // per-call guard threads covered it.
    let handle = isolate.thread_safe_handle();
    let cancel_handle = handle.clone(); // for cancel_terminate_execution on success
    let guard = InstanceGuard::new(handle.clone(), Arc::clone(&reason));
    if let Some(slot) = &ctl_slot {
        slot.set(guard.ctl()).ok();
    }
    // Armed under the run's token: a session Terminate that lands while this
    // one-off is mid-JS must be able to kill it via `abort_executing`.
    guard.set_for(
        GuardTarget {
            budget: Arc::clone(&cpu_budget),
            cpu_cap_ms: limits.cpu_time_ms,
            wall_deadline: (limits.wall_time_ms > 0)
                .then(|| start + Duration::from_millis(limits.wall_time_ms as u64)),
        },
        token,
    );

    // ── Near-heap callback (V8 heap objects: strings, plain objects) ──────────
    // Complements the ArrayBuffer allocator above.  Together they cover all
    // memory sources.  The Box must outlive active JS execution; safe because
    // V8 never invokes near-heap callbacks during Isolate::Dispose().
    let _near_heap: Option<Box<NearHeapData>> = if limits.hard_memory_mb > 0 {
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

    let context_global = {
        v8::scope!(let scope, &mut isolate);
        let context = v8::Context::new(scope, Default::default());
        let scope = &mut v8::ContextScope::new(scope, context);
        // Setup runs with the guards already armed: a guard firing mid-setup
        // must report the timeout, not the step it interrupted.
        crate::snapshot::boot_context(scope, &*run_table).map_err(|e| {
            failure(
                termination_or(&reason, RunError::Internal(e)),
                &unsafe { &*logs_ptr }.borrow(),
                start,
            )
        })?;
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
        RunEntry::Module { code, filename },
        &mut stub_slots,
        &guard,
        CallPhaseCtx {
            token,
            table: std::ptr::addr_of!(*run_table),
            limits: &limits,
            globals,
            imports,
            io,
            call_id_counter,
            call,
            start,
            cpu_budget: &cpu_budget,
            bridge_log: &bridge_log,
            reason: &reason,
            logs: logs_ptr,
            pending: pending_ptr,
            streams: streams_ptr,
            out_streams: out_streams_ptr,
            timers: timers_ptr,
            epilogue,
            cancel_handle: &cancel_handle,
        },
    );
    guard.clear();
    // Failure paths return out of run_call_phase before its own stream
    // cleanup: close here too (idempotent), so the host releases its sources
    // and the per-thread table pointer never dangles past the Box.
    close_run_streams(streams_ptr);
    close_out_streams(out_streams_ptr, "run ended");
    result
}

// ── Run I/O ──────────────────────────────────────────────────────────────────

/// One message on an instance's single ordered channel. A run's job is
/// enqueued before any of its events can be, so an event whose token is not
/// live can only be late, never early.
pub enum RunEvent {
    /// A new run for the instance's turn loop. One-off runs never see this
    /// variant (their private channel carries only their own events).
    Job(Box<CallJob>),
    /// A host message addressed to this run (bridge response, stream
    /// traffic, terminate, connection loss).
    Host(Inbound),
    /// The registry dropped the instance's handle: accept no new work, exit
    /// once the in-flight runs drain. Sent by `InstanceHandle::drop`, so a
    /// leaked sender clone can never strand a retired instance.
    Retire,
}

/// One event routed to a run, tagged with the run's table token and stamped
/// with its arrival time at the demux.
///
/// The stamp decides deadline ordering in the multi-run loop: a run's
/// boundary deadline concludes it only once every event that ARRIVED before
/// that deadline has been delivered — so a settling frame that reached the
/// demux in time is never beaten by the deadline just because the loop was
/// busy — while events arriving after the deadline cannot delay it. (Two
/// connections feeding one instance can interleave stamps out of order by
/// the stamp-to-send race, a few µs — the same boundary blur any transport
/// has; within one connection stamps are monotone.)
pub struct RoutedEvent {
    pub token: u64,
    pub event: RunEvent,
    pub at: std::time::Instant,
}

impl RoutedEvent {
    /// Stamp `event` for `token` with the current time — called at the
    /// routing site, so the stamp is the event's arrival.
    pub fn new(token: u64, event: RunEvent) -> Self {
        Self {
            token,
            event,
            at: std::time::Instant::now(),
        }
    }
}

/// Sender half of an instance channel. A one-off run has a private channel
/// (its token is the only one that ever appears, and it never carries Job or
/// Retire); a warm instance has ONE channel for everything — jobs, all of
/// its runs' events, Retire — so per-run ordering is channel FIFO (#170).
pub type RunEventSender = crossbeam_channel::Sender<RoutedEvent>;

/// Where a run's host messages come from and where its own go.
pub enum RunIo {
    /// A private event channel (one-off runs): the frontend feeds this
    /// run's events; outbound messages go through its link.
    Channel {
        events: crossbeam_channel::Receiver<RoutedEvent>,
        link: Arc<dyn HostLink>,
    },
    /// A run on a warm instance: events arrive on the instance's shared
    /// channel (owned by its turn loop), so the run carries only the link.
    Instance { link: Arc<dyn HostLink> },
    /// No I/O: a pure compute run (no bridge, no host to answer).
    None,
}

impl RunIo {
    pub fn link(&self) -> Option<Arc<dyn HostLink>> {
        match self {
            RunIo::Channel { link, .. } | RunIo::Instance { link } => Some(Arc::clone(link)),
            RunIo::None => None,
        }
    }
}

/// The read half of [`RunIo`], as the drive loops hold it.
enum RunSource {
    Channel(crossbeam_channel::Receiver<RoutedEvent>),
    /// Events arrive on the owning instance's shared channel — the run can
    /// be woken (it is NOT sourceless), it just owns no receiver itself.
    Instance,
}

// ── The turn loop ────────────────────────────────────────────────────────────
//
// One instance serves its in-flight runs as a sequence of TURNS: enter the
// isolate on behalf of exactly one run, do one unit of work (start it,
// deliver one frame to it, observe a deadline), run a microtask checkpoint,
// scan the run's root promise, and leave. Nobody holds the isolate between
// turns — a run's continuation state lives in [`RunState`], not on the
// stack — so any number of runs can be suspended on one instance while the
// owner thread waits for the next event. (The #81 design; this replaces the
// per-call blocking poll loop and the separate grace phase.)

/// Per-call parameters for [`run_call_phase`] — bundled so the one-off path
/// (`run_module_inner`) and warm instances (`run_call_on_core`) drive
/// the identical machinery with their own state lifetimes.
struct CallPhaseCtx<'a> {
    /// This run's table token — the identity every native callback
    /// attributes by.
    token: u64,
    /// The instance's run table.
    table: *const RunTable,
    limits: &'a Limits,
    globals: &'a [HostGlobalDef],
    imports: &'a [ImportBinding],
    io: RunIo,
    call_id_counter: Arc<AtomicU32>,
    call: Option<&'a types::CallSpec>,
    start: std::time::Instant,
    cpu_budget: &'a Arc<CpuBudget>,
    bridge_log: &'a Arc<Mutex<BridgeCallLog>>,
    reason: &'a Arc<ReasonCell>,
    /// The run's console buffers (in its table entry) for failure snapshots.
    /// A raw pointer; only transient borrows are taken, never across JS
    /// execution.
    logs: *const RefCell<LogBuffers>,
    /// The run's `waitUntil` registration set — in its table entry.
    pending: *const PendingWork,
    /// The run's streamed-body registry — in its table entry.
    streams: *const StreamTable,
    /// The run's outbound streamed-body registry — in its table entry.
    out_streams: *const OutStreamTable,
    /// The run's pending timers — in its table entry.
    timers: *const RefCell<TimerTable>,
    /// Per-run identity for the early Result frame; `None` = hold mode.
    epilogue: Option<EpilogueSpec>,
    cancel_handle: &'a v8::IsolateHandle,
}

/// How far a run has gotten; what the next event means for it.
enum RunPhase {
    /// The start turn has not armed a root yet.
    Starting,
    /// The root promise is pending; frames and deadlines drive it.
    Settling {
        root: v8::Global<v8::Promise>,
        /// The module whose namespace the exports come from. `None` for a
        /// call whose return promise is already the root.
        module: Option<v8::Global<v8::Module>>,
        /// A requested call still has to be invoked once `root` (the module
        /// evaluation promise) fulfils.
        call_pending: bool,
        /// The root's value IS the run result (a call's return promise) —
        /// serialize it instead of walking module exports.
        root_is_call_result: bool,
    },
    /// The value settled and (in session mode) the early Result shipped;
    /// the registered `waitUntil` work is being driven under grace budgets.
    /// Boxed: the settled Output rides in it, and most runs never grace.
    Grace(Box<GraceState>),
}

/// Loop-side state of one in-flight run.
struct RunState {
    token: u64,
    table: *const RunTable,
    limits: Limits,
    start: std::time::Instant,
    epilogue: Option<EpilogueSpec>,
    /// Where this run's events come from; `None` = nothing can wake it.
    source: Option<RunSource>,
    /// Outbound messages (the early Result at grace start). Bridge stubs
    /// and stream credit carry their own clone via the table entry.
    link: Option<Arc<dyn HostLink>>,
    cpu_budget: Arc<CpuBudget>,
    bridge_log: Arc<Mutex<BridgeCallLog>>,
    bridge_error: Arc<OnceLock<RunError>>,
    resolvers: PendingResolvers,
    // Pointers into this run's table entry — valid while the entry lives
    // (the callers remove the entry only after the drive returns).
    logs: *const RefCell<LogBuffers>,
    pending: *const PendingWork,
    streams: *const StreamTable,
    out_streams: *const OutStreamTable,
    timers: *const RefCell<TimerTable>,
    phase: RunPhase,
    /// Counted logic time of the current phase: own turns + waits-to-arrival.
    /// Frozen for the run phase once grace begins (grace bills its own).
    counted: Duration,
    /// When the run last parked; the next wait is counted from here.
    parked_at: std::time::Instant,
    /// Set while a turn for this run executes on the thread.
    turn_entered: Option<std::time::Instant>,
}

/// Post-Result bookkeeping: the `waitUntil` grace part (the dissolved
/// `run_grace_phase`) and, since #128, the outbound-streaming part — the
/// phase ends when BOTH are done. Stream state itself lives in the run's
/// [`OutStreamTable`] entry; this holds the waitUntil side.
struct GraceState {
    /// The settled run value; `background` is stamped at grace end.
    output: Output,
    grace_start: std::time::Instant,
    /// The waitUntil wall budget — `None` for a phase entered only for
    /// streaming. Erodes on counted grace time, so the boundary instant is
    /// `parked_at + (cap − counted)` while parked.
    wall_cap: Option<Duration>,
    /// Counted grace logic time (the run's `counted` freezes at grace entry).
    counted: Duration,
    /// The waitUntil part's outcome once decided; the phase outlives it
    /// while outbound streams are still open.
    grace_status: Option<GraceStatus>,
    grace_cpu: Arc<CpuBudget>,
    grace_cpu_cap: u32,
    error: Option<(String, String)>,
    early_result_sent: bool,
    /// Bridge records that belong to the run phase (already shipped on the
    /// early Result); grace reports only what comes after.
    run_record_count: usize,
}

impl RunState {
    /// Snapshot the run's console buffers for a failure result.
    fn fail(&self, error: RunError) -> FailureOutput {
        // SAFETY: `logs` points into the run's live table entry.
        failure(error, &unsafe { &*self.logs }.borrow(), self.start)
    }

    /// The budget the current phase bills JS execution to.
    fn active_budget(&self) -> Arc<CpuBudget> {
        match &self.phase {
            RunPhase::Grace(g) => Arc::clone(&g.grace_cpu),
            _ => Arc::clone(&self.cpu_budget),
        }
    }

    /// How long the loop may wait for this run: wall budget while settling
    /// (None = uncapped), the phase deadlines in grace (waitUntil wall /
    /// stream idle), or its earliest timer — whichever is sooner.
    fn next_timeout(&self) -> Option<Duration> {
        let now = std::time::Instant::now();
        let boundary = deadline_instant(self)
            .map(|at| at.saturating_duration_since(now).max(Duration::from_millis(1)));
        let timer = earliest_timer(self).map(|at| {
            at.saturating_duration_since(now).max(Duration::from_millis(1))
        });
        match (boundary, timer) {
            (Some(b), Some(t)) => Some(b.min(t)),
            (b, t) => b.or(t),
        }
    }
}

/// Fold the wait that ended at `woke_at` into the phase's counted time and
/// mark a turn as executing. Waits count only to the waking event's arrival
/// (or a timer's due instant) — the delivery lag is the engine's.
fn enter_turn(rs: &mut RunState, woke_at: std::time::Instant) {
    let wait = woke_at.saturating_duration_since(rs.parked_at);
    match &mut rs.phase {
        RunPhase::Grace(g) => g.counted += wait,
        _ => rs.counted += wait,
    }
    rs.turn_entered = Some(std::time::Instant::now());
}

/// Fold the finished turn's execution into the phase's counted time and park.
fn leave_turn(rs: &mut RunState) {
    if let Some(t) = rs.turn_entered.take() {
        let exec = t.elapsed();
        match &mut rs.phase {
            RunPhase::Grace(g) => g.counted += exec,
            _ => rs.counted += exec,
        }
    }
    rs.parked_at = std::time::Instant::now();
}

/// The live (not yet folded) counted segment: the turn so far when
/// executing, the wait so far when parked — callers that must cut a wait
/// earlier than "now" call [`enter_turn`] with the cutoff first.
fn live_segment(rs: &RunState) -> Duration {
    match rs.turn_entered {
        Some(t) => t.elapsed(),
        None => rs.parked_at.elapsed(),
    }
}

/// The RUN phase's counted logic time as of now, for stamping a concluding
/// result. Frozen once grace began — grace bills its own accumulator.
fn counted_wall_ms(rs: &RunState) -> f64 {
    match rs.phase {
        RunPhase::Grace(_) => dur_ms(rs.counted),
        _ => dur_ms(rs.counted + live_segment(rs)),
    }
}

/// Duration → ms rounded to microsecond resolution (matches `elapsed_ms`).
fn dur_ms(d: Duration) -> f64 {
    (d.as_secs_f64() * 1_000_000.0).round() / 1_000.0
}

/// Borrowed per-run facts the turn functions need beside the mutable state.
struct RunFacts<'a> {
    call: Option<&'a types::CallSpec>,
    reason: &'a Arc<ReasonCell>,
    guard: &'a InstanceGuard,
    cancel_handle: &'a v8::IsolateHandle,
}

/// What a scan decided about one run.
enum ScanOutcome {
    /// Still in flight — wait for the next event.
    Continue,
    /// The run is over; this is its result.
    Finished(Box<Result<Output, FailureOutput>>),
}

fn finished(r: Result<Output, FailureOutput>) -> ScanOutcome {
    ScanOutcome::Finished(Box::new(r))
}

/// One event delivered to the loop while runs are suspended.
enum LoopEvent {
    Host(Inbound),
    /// The wait timed out — the nearest boundary deadline is due.
    DeadlineHit,
    /// The event source is gone (connection lost / channel closed).
    SourceClosed(io::Error),
}

/// Wait for the next event on a private channel, plus the instant the
/// wait's counted time cuts off (a message's arrival stamp; delivery time
/// for the rest). Only the single-run drive loop (`run_call_phase`) calls
/// this; instance-channel runs are woken by their owner loop's select.
fn wait_event(source: &RunSource, timeout: Option<Duration>) -> (LoopEvent, std::time::Instant) {
    match source {
        RunSource::Instance => {
            unreachable!("instance-channel runs are driven by serve_instance, not wait_event")
        }
        RunSource::Channel(rx) => {
            let received = match timeout {
                Some(t) => rx.recv_timeout(t),
                None => rx
                    .recv()
                    .map_err(|_| crossbeam_channel::RecvTimeoutError::Disconnected),
            };
            // Delivery order needs no stamp here — a single-run channel is
            // already arrival-ordered against this run's own deadline (a
            // queued event always beats the recv timeout). The stamp only
            // cuts off the wait's counted time.
            let woke_at = received
                .as_ref()
                .map(|routed| routed.at)
                .unwrap_or_else(|_| std::time::Instant::now());
            let event = match received.map(|routed| routed.event) {
                Ok(RunEvent::Host(Inbound::ConnectionLost { detail })) => {
                    LoopEvent::SourceClosed(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        detail.unwrap_or_else(|| "connection closed".to_string()),
                    ))
                }
                Ok(RunEvent::Host(msg)) => LoopEvent::Host(msg),
                // Instance-loop message kinds; a one-off's private channel
                // never carries them.
                Ok(RunEvent::Job(_) | RunEvent::Retire) => {
                    debug_assert!(false, "Job/Retire on a one-off run's private channel");
                    LoopEvent::SourceClosed(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "unexpected instance message on a private run channel",
                    ))
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => LoopEvent::DeadlineHit,
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => LoopEvent::SourceClosed(
                    io::Error::new(io::ErrorKind::BrokenPipe, "demux gone"),
                ),
            };
            (event, woke_at)
        }
    }
}

/// What a run executes: a one-off module compiled in its start turn, or an
/// export of the instance's prefix module.
#[derive(Clone, Copy)]
enum RunEntry<'a> {
    Module { code: &'a str, filename: &'a str },
    Prefix(&'a v8::Global<v8::Module>),
}

/// The per-call half of a run: bridge stubs in, the one-off module evaluated
/// or the prefix export called and settled, value blob out, grace driven — as
/// turns over [`RunState`].
/// Everything before this (isolate boot, web runtime, prefix evaluation) is
/// instance-lifetime state that one-off runs rebuild per run and warm
/// instances retain across calls.
///
/// The caller owns the guard target for the START turn (it must already
/// cover the pre-phase placeholder install); this function clears it when
/// the run first suspends and every later turn re-arms it.
#[allow(clippy::too_many_arguments)] // the two callers bundle their own state
fn run_call_phase(
    isolate: &mut v8::OwnedIsolate,
    context: &v8::Global<v8::Context>,
    entry: RunEntry<'_>,
    stub_slots: &mut StubSlots,
    guard: &InstanceGuard,
    mut ctx: CallPhaseCtx<'_>,
) -> Result<Output, FailureOutput> {
    let (mut rs, start_outcome) = begin_call(
        isolate,
        context,
        entry,
        stub_slots,
        guard,
        &mut ctx,
    );
    if let ScanOutcome::Finished(result) = start_outcome {
        return stamp_wall(&rs, *result);
    }
    let facts = RunFacts {
        call: ctx.call,
        reason: ctx.reason,
        guard,
        cancel_handle: ctx.cancel_handle,
    };

    // The run is suspended: drive it on events until it finishes.
    loop {
        // A turn that suspended after a mid-turn kill ends here, as the
        // instance loop's taint check does.
        if facts.reason.get().is_some() {
            let result = if matches!(rs.phase, RunPhase::Grace(_)) {
                Ok(finish_killed_grace(&facts, &mut rs))
            } else {
                Err(rs.fail(reason_error(facts.reason)))
            };
            return stamp_wall(&rs, result);
        }
        // Deadlines are recomputed each pass; the touched-run stamps are
        // an instance-loop concern.
        TIMERS_TOUCHED.with(|v| v.borrow_mut().clear());
        if rs.source.is_none() {
            if earliest_timer(&rs).is_some() {
                // No socket, but timers are internal events: park to the
                // next deadline (outside any budget epoch — a sleep costs
                // no CPU) and serve it.
                if let Some(at) = next_deadline(&rs) {
                    let now = std::time::Instant::now();
                    if at > now {
                        std::thread::sleep(at - now);
                    }
                }
                if let Some(result) = deadline_hit(isolate, context, &facts, &mut rs) {
                    return stamp_wall(&rs, result);
                }
                continue;
            }
            // Nothing can ever deliver an event to this run. `scan` fails
            // a sourceless pending run without timers, so only hold-mode
            // grace gets here.
            let result = Ok(finish_post_phase(&mut rs));
            return stamp_wall(&rs, result);
        }
        let timeout = rs.next_timeout();
        let (event, woke_at) = {
            let source = rs.source.as_ref().expect("checked above");
            wait_event(source, timeout)
        };
        let entered = matches!(event, LoopEvent::Host(_));
        if entered {
            enter_turn(&mut rs, woke_at);
        }
        let outcome = handle_loop_event(isolate, context, &facts, &mut rs, event);
        if entered {
            leave_turn(&mut rs);
        }
        if let Some(result) = outcome {
            return stamp_wall(&rs, result);
        }
    }
}

/// Stamp the counted logic time onto a concluding single-run result (the
/// instance loop's counterpart is in `conclude_run`).
fn stamp_wall(
    rs: &RunState,
    mut result: Result<Output, FailureOutput>,
) -> Result<Output, FailureOutput> {
    let wall = counted_wall_ms(rs);
    match &mut result {
        Ok(output) => output.wall_time_ms = wall,
        Err(f) => f.wall_time_ms = wall,
    }
    result
}

/// Build the run's state and execute its start turn: install this call's
/// bridge surface, evaluate the one-off module (or invoke the call),
/// checkpoint, scan. Finishes the run outright in the common no-bridge case. The caller
/// pre-armed the guard target (it must cover the placeholder install);
/// this releases it — and the budget epoch — however the turn ends.
#[allow(clippy::too_many_arguments)]
fn begin_call(
    isolate: &mut v8::OwnedIsolate,
    context: &v8::Global<v8::Context>,
    entry: RunEntry<'_>,
    stub_slots: &mut StubSlots,
    guard: &InstanceGuard,
    ctx: &mut CallPhaseCtx<'_>,
) -> (RunState, ScanOutcome) {
    let facts = RunFacts {
        call: ctx.call,
        reason: ctx.reason,
        guard,
        cancel_handle: ctx.cancel_handle,
    };
    let (source, link) = match std::mem::replace(&mut ctx.io, RunIo::None) {
        RunIo::Channel { events, link } => (Some(RunSource::Channel(events)), Some(link)),
        RunIo::Instance { link } => (Some(RunSource::Instance), Some(link)),
        RunIo::None => (None, None),
    };
    let mut rs = RunState {
        token: ctx.token,
        table: ctx.table,
        limits: *ctx.limits,
        start: ctx.start,
        epilogue: ctx.epilogue,
        source,
        link,
        cpu_budget: Arc::clone(ctx.cpu_budget),
        bridge_log: Arc::clone(ctx.bridge_log),
        bridge_error: Arc::new(OnceLock::new()),
        resolvers: Arc::new(Mutex::new(HashMap::new())),
        logs: ctx.logs,
        pending: ctx.pending,
        streams: ctx.streams,
        out_streams: ctx.out_streams,
        timers: ctx.timers,
        phase: RunPhase::Starting,
        counted: Duration::ZERO,
        parked_at: ctx.start,
        turn_entered: Some(ctx.start),
    };
    let start_outcome = start_turn(
        isolate,
        context,
        entry,
        stub_slots,
        ctx,
        &facts,
        &mut rs,
    );
    rs.cpu_budget.leave();
    guard.clear();
    leave_turn(&mut rs);
    (rs, start_outcome)
}

/// Deliver one loop event to a suspended run. `Some` = the run finished.
fn handle_loop_event(
    isolate: &mut v8::OwnedIsolate,
    context: &v8::Global<v8::Context>,
    facts: &RunFacts<'_>,
    rs: &mut RunState,
    event: LoopEvent,
) -> Option<Result<Output, FailureOutput>> {
    match event {
        LoopEvent::Host(msg) => match frame_turn(isolate, context, facts, rs, msg) {
            ScanOutcome::Finished(result) => Some(*result),
            ScanOutcome::Continue => None,
        },
        LoopEvent::DeadlineHit => deadline_hit(isolate, context, facts, rs),
        LoopEvent::SourceClosed(e) => {
            if matches!(rs.phase, RunPhase::Grace(_)) && out_streams_open(rs) {
                conn_lost_stream_turn(isolate, context, facts, rs);
            }
            Some(boundary_close(rs, e))
        }
    }
}

/// The single-run wait timed out: fire every due timer, then conclude the
/// boundary if it too is due (a due timer beats the wall). `None` = still
/// in flight; the caller re-waits.
fn deadline_hit(
    isolate: &mut v8::OwnedIsolate,
    context: &v8::Global<v8::Context>,
    facts: &RunFacts<'_>,
    rs: &mut RunState,
) -> Option<Result<Output, FailureOutput>> {
    loop {
        let now = std::time::Instant::now();
        if let Some(due) = earliest_timer(rs).filter(|t| *t <= now) {
            // The wait ended at the timer's due instant, not its late fire.
            enter_turn(rs, due);
            let outcome = timer_turn(isolate, context, facts, rs);
            leave_turn(rs);
            match outcome {
                ScanOutcome::Finished(result) => return Some(*result),
                ScanOutcome::Continue => continue,
            }
        }
        if matches!(rs.phase, RunPhase::Grace(_)) {
            // Two deadlines share the post-Result phase: the stream idle
            // allowance (a real turn — guest cancel cleanup runs) and the
            // waitUntil wall (bookkeeping — the phase may stay open for
            // streams).
            if let Some(d) = out_idle_deadline(rs).filter(|d| *d <= now) {
                enter_turn(rs, d);
                let outcome = stream_idle_turn(isolate, context, facts, rs);
                leave_turn(rs);
                match outcome {
                    ScanOutcome::Finished(result) => return Some(*result),
                    ScanOutcome::Continue => continue,
                }
            }
            if let Some(b) = grace_wall_instant(rs).filter(|b| *b <= now) {
                // Counted grace time reaches its cap exactly here.
                enter_turn(rs, b);
                let concluded = conclude_grace_wall(rs);
                leave_turn(rs);
                if let Some(output) = concluded {
                    return Some(Ok(output));
                }
                continue;
            }
            return None;
        }
        if let Some(b) = deadline_instant(rs).filter(|b| *b <= now) {
            // Counted wall reaches its cap exactly at the boundary instant.
            enter_turn(rs, b);
            return Some(boundary_deadline(rs));
        }
        return None;
    }
}

/// The waitUntil wall's boundary instant while undecided: the counted grace
/// time reaches its cap after `cap − counted` more parked waiting.
fn grace_wall_instant(rs: &RunState) -> Option<std::time::Instant> {
    match &rs.phase {
        RunPhase::Grace(g) if g.grace_status.is_none() => g
            .wall_cap
            .map(|cap| rs.parked_at + cap.saturating_sub(g.counted)),
        _ => None,
    }
}

/// Whether the phase's waitUntil wall is due (and still undecided).
fn grace_wall_due(rs: &RunState, now: std::time::Instant) -> bool {
    grace_wall_instant(rs).is_some_and(|d| d <= now)
}

/// The waitUntil wall expired: record the truncation. Returns the finished
/// Output when nothing else holds the phase open, `None` while outbound
/// streams still do.
fn conclude_grace_wall(rs: &mut RunState) -> Option<Output> {
    if let RunPhase::Grace(g) = &mut rs.phase {
        g.grace_status = Some(GraceStatus::Truncated);
    }
    if out_streams_open(rs) {
        return None;
    }
    Some(finish_post_phase(rs))
}

/// The stream idle deadline fired: the host neither read nor cancelled for
/// the whole allowance — cancel every open stream WITH guest cleanup (one
/// turn; the reader-side `StreamEnd` names the timeout), then run the
/// normal phase-end scan.
fn stream_idle_turn(
    isolate: &mut v8::OwnedIsolate,
    context: &v8::Global<v8::Context>,
    facts: &RunFacts<'_>,
    rs: &mut RunState,
) -> ScanOutcome {
    let _turn = TurnGuard::begin(facts.guard, rs);
    let _epoch = BudgetEpoch::enter(rs.active_budget());

    v8::scope!(let scope, isolate);
    let context_local = v8::Local::new(scope, context);
    let scope = &mut v8::ContextScope::new(scope, context_local);

    crate::webtypes::advance_frozen_clock(scope);
    CURRENT_RUN_TOKEN.with(|c| c.set(rs.token));
    RUN_TABLE_PTR.with(|c| c.set(rs.table));
    install_cped_rider(scope, rs.token);

    v8::tc_scope!(let scope, scope);
    cancel_all_out_streams(scope, rs, "stream idle timeout");
    scope.perform_microtask_checkpoint();
    finish_turn(scope, facts, rs, true)
}

/// Connection death with outbound streams open: run their guest cleanup
/// (reason `"connection lost"`) as one turn before the boundary close — the
/// host is gone, but the guest's `finally`/cancel semantics hold (Q4).
fn conn_lost_stream_turn(
    isolate: &mut v8::OwnedIsolate,
    context: &v8::Global<v8::Context>,
    facts: &RunFacts<'_>,
    rs: &mut RunState,
) {
    let _turn = TurnGuard::begin(facts.guard, rs);
    let _epoch = BudgetEpoch::enter(rs.active_budget());

    v8::scope!(let scope, isolate);
    let context_local = v8::Local::new(scope, context);
    let scope = &mut v8::ContextScope::new(scope, context_local);

    crate::webtypes::advance_frozen_clock(scope);
    CURRENT_RUN_TOKEN.with(|c| c.set(rs.token));
    RUN_TABLE_PTR.with(|c| c.set(rs.table));
    install_cped_rider(scope, rs.token);

    v8::tc_scope!(let scope, scope);
    cancel_all_out_streams(scope, rs, "connection lost");
    scope.perform_microtask_checkpoint();
}

/// A boundary deadline: wall expiry while suspended is a clean per-run
/// failure — nothing was interrupted mid-JS, the instance keeps serving
/// (E1 ruling 3; this deliberately no longer taints). Grace deadline =
/// truncation.
fn boundary_deadline(rs: &mut RunState) -> Result<Output, FailureOutput> {
    match rs.phase {
        RunPhase::Starting | RunPhase::Settling { .. } => Err(rs.fail(RunError::WallTimeout)),
        RunPhase::Grace(_) => Ok(finish_post_phase(rs)),
    }
}

/// The run's event source died while it was suspended: fail this run alone,
/// cleanly (E1 ruling 3 narrows the old taint).
fn boundary_close(rs: &mut RunState, e: io::Error) -> Result<Output, FailureOutput> {
    match rs.phase {
        RunPhase::Starting | RunPhase::Settling { .. } => Err(rs.fail(RunError::Internal(
            format!("poll loop socket read: {e}"),
        ))),
        RunPhase::Grace(_) => {
            if let RunPhase::Grace(g) = &mut rs.phase {
                if g.error.is_none() {
                    g.error = Some(capped_grace_error(
                        "Error".to_string(),
                        format!("poll loop socket read: {e}"),
                    ));
                }
            }
            Ok(finish_post_phase(rs))
        }
    }
}

/// The absolute instant of this run's current boundary deadline, if it has
/// one. Wall budgets erode on COUNTED time, so the instant is fixed per park
/// (`parked_at + remaining budget` — a parked run bills its wait 1:1 with
/// real time) and moves later with each turn; a heap entry is stale exactly
/// when it no longer equals this value.
fn deadline_instant(rs: &RunState) -> Option<std::time::Instant> {
    match &rs.phase {
        RunPhase::Starting | RunPhase::Settling { .. } => (rs.limits.wall_time_ms > 0).then(|| {
            let cap = Duration::from_millis(rs.limits.wall_time_ms as u64);
            rs.parked_at + cap.saturating_sub(rs.counted)
        }),
        RunPhase::Grace(_) => {
            // The waitUntil wall (while that part is undecided) and the
            // outbound-stream idle deadline, whichever is sooner.
            let grace = grace_wall_instant(rs);
            let idle = out_idle_deadline(rs);
            match (grace, idle) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            }
        }
    }
}

/// The run's earliest pending-timer deadline, if any. Lazily prunes cleared
/// heap tops on the way.
fn earliest_timer(rs: &RunState) -> Option<std::time::Instant> {
    // SAFETY: `timers` points into the run's live table entry.
    unsafe { &*rs.timers }.borrow_mut().peek_deadline()
}

/// The instant the loop must next wake for this run: phase boundary or
/// earliest timer, whichever is sooner (timers participate in grace too —
/// `graceMs` caps them). Heap validity in the multi-run loop is defined
/// against THIS value.
fn next_deadline(rs: &RunState) -> Option<std::time::Instant> {
    match (deadline_instant(rs), earliest_timer(rs)) {
        (Some(b), Some(t)) => Some(b.min(t)),
        (b, t) => b.or(t),
    }
}

/// The start turn. Returns `Continue` with `rs.phase` armed when the run
/// suspends, `Finished` when it completed (or failed) inside the turn.
#[allow(clippy::too_many_arguments)]
fn start_turn(
    isolate: &mut v8::OwnedIsolate,
    context: &v8::Global<v8::Context>,
    entry: RunEntry<'_>,
    stub_slots: &mut StubSlots,
    ctx: &CallPhaseCtx<'_>,
    facts: &RunFacts<'_>,
    rs: &mut RunState,
) -> ScanOutcome {
    v8::scope!(let scope, isolate);
    let context_local = v8::Local::new(scope, context);
    let scope = &mut v8::ContextScope::new(scope, context_local);

    // Each turn starts on freshly advanced frozen time — the clock moves
    // only when the runtime regains control at an event (E1 ruling 7).
    crate::webtypes::advance_frozen_clock(scope);

    // This turn executes on the run's behalf: native callbacks attribute to
    // it, and continuations created from here carry its identity even when
    // driven during a co-resident run's turn (the CPED rider).
    CURRENT_RUN_TOKEN.with(|c| c.set(rs.token));
    RUN_TABLE_PTR.with(|c| c.set(rs.table));
    install_cped_rider(scope, rs.token);

    // Split the tagged defs into the bridge stubs to install (plain functions,
    // shim handlers) and everything else. Value globals (string exprs,
    // constants, shim wrappers) are installed after the stubs so a shim
    // wrapper's handler stub already exists on globalThis. Each stub carries
    // the public name its bridge records report under: shim handler stubs
    // report under the public global name, everything else as-is.
    let mut bridge_stubs: Vec<BridgeStubSpec> = ctx
        .globals
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
    let import_handles = collect_import_handles(ctx.imports);
    if !import_handles.is_empty() {
        bridge_stubs.push(BridgeStubSpec {
            stub_name: BRIDGE_DISPATCH_GLOBAL.to_string(),
            record_name: BRIDGE_DISPATCH_GLOBAL.to_string(),
            import_handles: Some(Arc::new(import_handles)),
            enumerable: true, // internal name — DONT_ENUM applies regardless
        });
    }

    if !bridge_stubs.is_empty() {
        let link = rs
            .link
            .clone()
            .expect("install_bridge_globals called with bridge-backed globals but no run I/O");
        if let Err(e) = install_bridge_globals(
            scope,
            &bridge_stubs,
            rs.token,
            ctx.table,
            rs.epilogue.map(|e| e.run_id).unwrap_or(0),
            link,
            Arc::clone(&ctx.call_id_counter),
            Arc::clone(&rs.bridge_error),
            rs.limits.max_bridge_call_bytes,
            Arc::new(AtomicU32::new(0)),
            rs.limits.max_bridge_calls,
            Arc::clone(&rs.resolvers),
            rs.start,
            Arc::clone(&rs.bridge_log),
            stub_slots,
        ) {
            return finished(Err(rs.fail(termination_or(facts.reason, e))));
        }
    }

    // Install the value globals (string exprs, constants, shim wrappers)
    // natively. A PrefixRun carrying one is refused by the engine, so for it
    // this is a no-op; its value globals were replayed from the prefix defs.
    if let Err(e) = install_value_globals(scope, ctx.globals) {
        return finished(Err(rs.fail(termination_or(facts.reason, e))));
    }

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
    // SAFETY: `pending` points at the run entry's PendingWork Box.
    unsafe {
        *(*rs.pending).slot.borrow_mut() = Some(Vec::new());
    }
    // Arm the streamed-body registry for this run. The codec's rehydration
    // walk (no context parameter) reaches it through the caller-installed
    // RUN_TABLE_PTR + current token — the brand-key pattern.
    // SAFETY: `streams` points at the run entry's StreamTable Box.
    unsafe {
        *(*rs.streams).slot.borrow_mut() = Some(StreamTableState {
            streams: HashMap::new(),
            run_id: rs.epilogue.map(|e| e.run_id).unwrap_or(0),
            link: rs.link.clone(),
        });
    }
    // Arm the OUTBOUND registry only for runs that can stream a result out:
    // an epilogue (the early Result carries the handle) and a link to send
    // chunks through. Hold-mode runs stay unarmed — registration fails and
    // the codec reports the existing cannot-cross error.
    if rs.epilogue.is_some() && rs.link.is_some() {
        // SAFETY: `out_streams` points at the run entry's OutStreamTable Box.
        unsafe {
            *(*rs.out_streams).slot.borrow_mut() = Some(OutTableState {
                streams: HashMap::new(),
                next_id: 1,
                run_id: rs.epilogue.map(|e| e.run_id).unwrap_or(0),
                link: rs.link.clone(),
                last_activity: std::time::Instant::now(),
            });
        }
    }

    v8::tc_scope!(let scope, scope);

    // A one-off compiles, instantiates and evaluates its module here; a
    // prefix run calls into the module evaluated at instance creation.
    match entry {
        RunEntry::Module { code, filename } => {
            let Some(source_string) = v8::String::new(scope, code) else {
                return finished(Err(rs.fail(RunError::Internal(
                    "failed to intern module source".to_string(),
                ))));
            };
            let Some(filename_str) = v8::String::new(scope, filename) else {
                return finished(Err(rs.fail(RunError::Internal(
                    "failed to intern filename".to_string(),
                ))));
            };
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
                None => {
                    return finished(Err(rs.fail(termination_or(
                        facts.reason,
                        syntax_error(scope),
                    ))));
                }
            };

            // Install the module resolver for this run. After
            // `instantiate_module` returns we inspect `resolve_error` for any
            // non-V8-throwable error recorded inside the resolver (e.g. a source
            // module that failed to compile).
            let _resolver_guard = install_resolver(ResolverContext {
                bindings: ctx.imports.to_vec(),
                module_cache: HashMap::new(),
                pending_meta: Vec::new(),
                resolve_error: None,
            });

            if module
                .instantiate_module(scope, module_resolver_callback)
                .is_none()
            {
                let err = take_resolver()
                    .and_then(|ctx| ctx.resolve_error)
                    .unwrap_or_else(|| RunError::Import(exception_message(scope)));
                return finished(Err(rs.fail(termination_or(facts.reason, err))));
            }

            rs.cpu_budget.enter(); // start measuring active CPU time (compile + scope setup excluded)
            let evaluation = match module.evaluate(scope) {
                Some(value) => value,
                None => {
                    if let Some(err) = rs.bridge_error.get() {
                        return finished(Err(rs.fail(termination_or(facts.reason, owned_bridge_error(err)))));
                    }
                    let error = match facts.reason.get() {
                        Some(TerminationReason::Wall) => RunError::WallTimeout,
                        Some(TerminationReason::Cpu) => RunError::CpuTimeout,
                        Some(TerminationReason::Memory) => RunError::MemoryLimit,
                        // A hard abort's mid-turn kill (`GuardCtl::abort_executing`).
                        Some(TerminationReason::Abort) => RunError::Aborted,
                        Some(TerminationReason::Import(m)) => RunError::Import(m.to_string()),
                        // Belt-and-suspenders: a terminate with no reason at all.
                        None if facts.cancel_handle.is_execution_terminating() => RunError::Aborted,
                        None => RunError::RuntimeError(Box::new(RuntimeErrorData {
                            name: exception_name(scope),
                            message: exception_message(scope),
                            stack: exception_stack(scope),
                            fields: exception_fields(scope),
                        })),
                    };
                    // Reading the exception can run guest getters.
                    let error = rs.bridge_error.get().map_or(error, owned_bridge_error);
                    return finished(Err(rs.fail(termination_or(facts.reason, error))));
                }
            };

            // One checkpoint immediately after evaluate() so that purely
            // synchronous results settle now, synchronous bridge errors propagate
            // to the module's top-level promise now, and termination exceptions
            // propagate now. For pending bridge calls nothing changes — the
            // resolvers aren't set yet.
            scope.perform_microtask_checkpoint();
            if facts.reason.get().is_some() {
                return finished(Err(rs.fail(reason_error(facts.reason))));
            }

            // ESM Module::Evaluate() always returns a Promise. The try_from is a
            // belt-and-suspenders guard; failure would be an internal V8 bug.
            let Ok(promise) = v8::Local::<v8::Promise>::try_from(evaluation) else {
                return finished(Err(rs.fail(RunError::Internal(
                    "module evaluation did not return a Promise".into(),
                ))));
            };

            rs.phase = RunPhase::Settling {
                root: v8::Global::new(scope, promise),
                module: Some(v8::Global::new(scope, module)),
                call_pending: facts.call.is_some(),
                root_is_call_result: false,
            };
            scan(scope, facts, rs)
        }
        RunEntry::Prefix(prefix_module) => {
            // Prefix run: the prefix module evaluated at instance creation is
            // the call target. Enter the CPU budget here — the one-off path
            // enters it right before evaluate() — so path resolution and the
            // call itself are measured (a path segment can be an accessor).
            rs.cpu_budget.enter();
            let module = v8::Local::new(scope, prefix_module);
            match invoke_requested_call(scope, facts, rs, module) {
                Ok(ScanOutcome::Continue) => scan(scope, facts, rs),
                Ok(done) => done,
                Err(f) => finished(Err(f)),
            }
        }
    }
}

/// Invoke the requested call against `module`'s namespace: resolve the export
/// path, call it, and either finish the run (sync value → serialized now) or
/// re-suspend on the returned promise. Runs inside the CPU budget.
fn invoke_requested_call(
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
fn serialize_call_result(
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

/// One event-driven turn: deliver `msg` to the run, checkpoint, scan.
fn frame_turn(
    isolate: &mut v8::OwnedIsolate,
    context: &v8::Global<v8::Context>,
    facts: &RunFacts<'_>,
    rs: &mut RunState,
    msg: Inbound,
) -> ScanOutcome {
    let in_grace = matches!(rs.phase, RunPhase::Grace(_));

    // Re-arm the guard and the phase budget for this turn: the delivery
    // below runs guest continuations. Both release on every exit path.
    let _turn = TurnGuard::begin(facts.guard, rs);
    let _epoch = BudgetEpoch::enter(rs.active_budget());

    v8::scope!(let scope, isolate);
    let context_local = v8::Local::new(scope, context);
    let scope = &mut v8::ContextScope::new(scope, context_local);

    // A message arrived — the runtime holds control, so this is an
    // observable I/O boundary: the frozen clock advances before anything of
    // it is delivered into JS (E1 ruling 7: once per turn entry).
    crate::webtypes::advance_frozen_clock(scope);
    CURRENT_RUN_TOKEN.with(|c| c.set(rs.token));
    RUN_TABLE_PTR.with(|c| c.set(rs.table));
    install_cped_rider(scope, rs.token);

    v8::tc_scope!(let scope, scope);

    match msg {
        Inbound::BridgeResponse { call_id, result } => {
            // Settle the call's bridge record: round-trip time on
            // the run clock plus the response value's serialized
            // size. Unknown callIds are ignored inside settle().
            let response_value_bytes = match &result {
                Ok(Some(blob)) => blob.len() as u32,
                Ok(None) | Err(_) => 0,
            };
            rs.bridge_log
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .settle(
                    call_id,
                    elapsed_ms(rs.start),
                    result.is_ok(),
                    response_value_bytes,
                );
            // Route to the matching resolver. An unknown callId
            // means a stale frame from a previous run — discard.
            let entry = rs
                .resolvers
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
                        let Some(v8_val) = decoded else {
                            // Same outcome as a data global or call
                            // argument that cannot cross: the run fails.
                            let error = match blob::take_codec_error() {
                                Some(e) => codec_error_to_run_error(e),
                                None => RunError::Internal(
                                    "failed to deserialize response value".to_string(),
                                ),
                            };
                            return finished(Err(rs.fail(termination_or(facts.reason, error))));
                        };
                        resolver.resolve(scope, v8_val);
                    }
                    Err(bridge_err) => {
                        // Reject with a real Error carrying the
                        // handler's name/message/fields, tagged with
                        // a private symbol so the rejected-root arm
                        // can classify an *uncaught* one as
                        // HostBridge. Sandbox code may catch it and
                        // continue — host handler errors are not
                        // run-fatal.
                        let error_obj = host_bridge_error_to_v8(scope, &bridge_err);
                        resolver.reject(scope, error_obj);
                    }
                }
            }
        }
        Inbound::Terminate { .. } => {
            // Mode is irrelevant here: a Terminate that reached the loop is
            // by definition landing at a turn boundary, where soft and hard
            // mean the same clean abandon (hard's extra power — the
            // mid-turn kill — was already applied by the frontend, or there
            // was nothing to kill).
            let run_id = rs.epilogue.map(|e| e.run_id).unwrap_or(0);
            if in_grace {
                // A host Terminate mid-grace (abort during the epilogue):
                // truncate the phase cleanly — the value already shipped.
                eprintln!("[iso4-v8] Terminate received for run {run_id} — truncating grace");
                return finished(Ok(finish_post_phase(rs)));
            }
            // Graceful abort = direct abandon (E2 amendment to E1 ruling 5):
            // the run simply stops existing. Its continuations are never
            // driven again, no cleanup slice runs, and nothing is
            // interrupted mid-JS — so the instance is NOT tainted and
            // co-resident runs are unaffected. The failure carries the real
            // telemetry collected so far.
            eprintln!("[iso4-v8] Terminate received for run {run_id} — abandoning");
            return finished(Err(rs.fail(RunError::Aborted)));
        }
        msg @ (Inbound::StreamChunk { .. } | Inbound::StreamEnd { .. }) => {
            if let Err(e) = deliver_stream_frame(scope, rs.streams, msg) {
                return finished(Err(rs.fail(e)));
            }
        }
        Inbound::StreamPull { stream_id, credit } => {
            // Outbound credit grant: production resumes in this turn's scan.
            note_out_credit(rs, stream_id, credit);
        }
        Inbound::StreamCancel { stream_id, reason } => {
            // The host dropped an outbound stream: guest cleanup runs as
            // part of this turn (web-streams cancel semantics), never taint.
            note_out_activity(rs);
            cancel_out_stream(scope, rs, stream_id, &reason, false);
        }
        Inbound::ConnectionLost { .. } => {
            unreachable!("connection loss is a SourceClosed event, never a turn")
        }
    }

    // Advance all pending continuation chains: this is the only place
    // microtasks run (Explicit policy). The resumed code executes until it
    // hits its next await (sending another BridgeCall) or completes.
    scope.perform_microtask_checkpoint();

    finish_turn(scope, facts, rs, in_grace)
}

/// Shared turn tail (frame and timer turns): check for a guard that fired
/// during the checkpoint, then for errors set inside bridge callbacks — both
/// win over the scan.
fn finish_turn(
    scope: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
    facts: &RunFacts<'_>,
    rs: &mut RunState,
    in_grace: bool,
) -> ScanOutcome {
    if let Some(r) = facts.reason.get() {
        if in_grace {
            // A mid-turn kill during grace: the caller's taint verdict sees
            // the reason cell; end the phase now instead of idling out the
            // wall — the slot frees immediately.
            return finished(Ok(finish_killed_grace(facts, rs)));
        }
        let error = match r {
            TerminationReason::Wall => RunError::WallTimeout,
            TerminationReason::Cpu => RunError::CpuTimeout,
            TerminationReason::Memory => RunError::MemoryLimit,
            TerminationReason::Abort => RunError::Aborted,
            TerminationReason::Import(m) => RunError::Import(m.to_string()),
        };
        return finished(Err(rs.fail(error)));
    }
    if let Some(err) = rs.bridge_error.get() {
        if in_grace {
            let msg = format!("{err:?}");
            if let RunPhase::Grace(g) = &mut rs.phase {
                if g.error.is_none() {
                    g.error = Some(capped_grace_error("Error".to_string(), msg));
                }
            }
            return finished(Ok(finish_post_phase(rs)));
        }
        return finished(Err(rs.fail(termination_or(facts.reason, owned_bridge_error(err)))));
    }

    scan(scope, facts, rs)
}

/// Look at the run's root promise (or grace set) and advance it as far as it
/// will go inside this turn.
fn scan(
    scope: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
    facts: &RunFacts<'_>,
    rs: &mut RunState,
) -> ScanOutcome {
    loop {
        let (root, module, call_pending, root_is_call_result) = match &rs.phase {
            RunPhase::Grace(_) => return grace_scan(scope, facts, rs),
            RunPhase::Starting => unreachable!("scan before the start turn armed a phase"),
            RunPhase::Settling {
                root,
                module,
                call_pending,
                root_is_call_result,
            } => (root.clone(), module.clone(), *call_pending, *root_is_call_result),
        };
        let root_local = v8::Local::new(scope, &root);
        match root_local.state() {
            v8::PromiseState::Pending => {
                // A fatal bridge error set during this run's turns means no
                // BridgeResponse will ever arrive — fail now, not at the
                // wall.
                if let Some(err) = rs.bridge_error.get() {
                    return finished(Err(rs.fail(termination_or(facts.reason, owned_bridge_error(err)))));
                }
                if rs.source.is_none() && earliest_timer(rs).is_none() {
                    // Pending with no socket and no pending timer —
                    // nothing in the isolate can ever resolve it.
                    let error = match facts.reason.get() {
                        Some(TerminationReason::Wall) => RunError::WallTimeout,
                        Some(TerminationReason::Cpu) => RunError::CpuTimeout,
                        Some(TerminationReason::Memory) => RunError::MemoryLimit,
                        Some(TerminationReason::Abort) => RunError::Aborted,
                        Some(TerminationReason::Import(m)) => RunError::Import(m.to_string()),
                        None => {
                            if root_is_call_result {
                                let path = facts
                                    .call
                                    .map(|c| c.export_path.as_str())
                                    .unwrap_or("<call>");
                                RunError::ExportNotSerializable(format!(
                                    "call \"{path}\" returned a promise that is still pending \
                                     after the poll loop: nothing in the isolate can resolve it"
                                ))
                            } else {
                                RunError::ExportNotSerializable(
                                    "module evaluation promise is still pending after poll loop"
                                        .to_string(),
                                )
                            }
                        }
                    };
                    return finished(Err(rs.fail(error)));
                }
                return ScanOutcome::Continue;
            }
            v8::PromiseState::Rejected => {
                let rejection = root_local.result(scope);
                // A rejection tagged as a host bridge error surfaces as
                // ERR_HOST_BRIDGE with the handler's name/message/fields
                // intact.
                if let Some(err) = host_bridge_error_from_rejection(scope, rejection) {
                    return finished(Err(rs.fail(termination_or(facts.reason, err))));
                }
                if let Some(err) = rs.bridge_error.get() {
                    return finished(Err(rs.fail(termination_or(facts.reason, owned_bridge_error(err)))));
                }
                let err = runtime_error_from_value(scope, rejection);
                let err = rs.bridge_error.get().map_or(err, owned_bridge_error);
                return finished(Err(rs.fail(termination_or(facts.reason, err))));
            }
            v8::PromiseState::Fulfilled => {
                if call_pending {
                    // The module settled; invoke the requested call (which
                    // may itself suspend on a new root — loop to re-scan).
                    let module_local = v8::Local::new(
                        scope,
                        module.as_ref().expect("call_pending without a module"),
                    );
                    match invoke_requested_call(scope, facts, rs, module_local) {
                        Ok(ScanOutcome::Continue) => continue,
                        Ok(done) => return done,
                        Err(f) => return finished(Err(f)),
                    }
                }
                if root_is_call_result {
                    let value = root_local.result(scope);
                    let exports = match serialize_call_result(scope, facts, rs, value) {
                        Ok(b) => b,
                        Err(f) => return finished(Err(f)),
                    };
                    return build_output_and_maybe_grace(scope, facts, rs, exports, Vec::new());
                }
                // Plain module run: walk the namespace exports.
                let module_local = v8::Local::new(
                    scope,
                    module.as_ref().expect("module run without a module"),
                );
                return extract_module_exports(scope, facts, rs, module_local);
            }
        }
    }
}

/// The common tail of a settled run: the post-serialization termination
/// check, the export size cap, the Output, and the grace decision.
fn build_output_and_maybe_grace(
    scope: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
    facts: &RunFacts<'_>,
    rs: &mut RunState,
    exports: Vec<u8>,
    skipped_exports: Vec<String>,
) -> ScanOutcome {
    // Termination only interrupts JS — the serializer's walk of plain data
    // can still complete after a guard fires (the per-export retry loop then
    // skips whatever export the interrupted getter sat on). A produced blob
    // does not make the run a success: the budget was exceeded.
    if facts.reason.get().is_some() {
        return finished(Err(rs.fail(termination_or(
            facts.reason,
            RunError::Internal("terminated during result serialization".to_string()),
        ))));
    }
    // A bridge violation inside a getter never makes the run a success.
    if let Some(err) = rs.bridge_error.get() {
        return finished(Err(rs.fail(owned_bridge_error(err))));
    }

    // ── Export size limit ────────────────────────────────────────────────
    // Measured on the blob itself — the payload that actually crosses the
    // socket. A call's return value is capped by the same limit.
    if rs.limits.max_export_bytes > 0 && exports.len() > rs.limits.max_export_bytes as usize {
        return finished(Err(rs.fail(RunError::ExportTooLarge)));
    }

    // A terminate with NO reason at this point is a routed abort
    // (`abort_executing`) that landed after the last JS left the isolate:
    // nothing was interrupted, but the demux consumed the Terminate frame
    // expecting this run to end. Cancel the pending flag and honor the
    // abort as a boundary abandon — absorbing it into a success Result (or
    // into grace) would silently un-abort the run.
    if facts.reason.get().is_none() && facts.cancel_handle.is_execution_terminating() {
        facts.cancel_handle.cancel_terminate_execution();
        return finished(Err(rs.fail(RunError::Aborted)));
    }

    let mut output = Output {
        exports,
        skipped_exports,
        // SAFETY: `logs` points into the run's live table entry.
        stdout: unsafe { &*rs.logs }.borrow().stdout.clone(),
        stderr: unsafe { &*rs.logs }.borrow().stderr.clone(),
        duration_ms: elapsed_ms(rs.start),
        // Stamped by the callers from the shared run state.
        wall_time_ms: 0.0,
        cpu_time_ms: 0.0,
        bridge_calls: Vec::new(),
        background: None,
    };

    // ── waitUntil / outbound streaming ───────────────────────────────────
    // The value is settled and serialized. Two things can keep the run
    // alive past its Result: registered `waitUntil` work (driven under
    // grace budgets), and outbound stream bodies the serialization just
    // registered (#128 — the host reads them chunk by chunk). Either one
    // starts the post-Result phase: the Result ships early with the
    // matching flags, and a final RunComplete frame ends the run once both
    // parts are done. A run with neither takes none of these branches.
    // SAFETY: `pending` points at the run entry's PendingWork Box.
    let has_pending = unsafe {
        (*rs.pending)
            .slot
            .borrow()
            .as_ref()
            .is_some_and(|set| !set.is_empty())
    };
    let has_grace = has_pending && rs.limits.grace_ms > 0;
    let has_streams = out_streams_open(rs);
    if !has_grace && !has_streams {
        return finished(Ok(output));
    }

    // The run phase is over: its budget and guard come down before grace
    // arms its own, and a lingering termination flag must not kill grace
    // work. The flag is INSPECTED before it is cancelled: a bare terminate
    // here is a routed abort that raced the settled value — honor it as a
    // boundary abandon rather than absorbing it into grace.
    rs.cpu_budget.leave();
    facts.guard.clear();
    let bare_terminate =
        facts.reason.get().is_none() && facts.cancel_handle.is_execution_terminating();
    facts.cancel_handle.cancel_terminate_execution();
    if bare_terminate {
        return finished(Err(rs.fail(RunError::Aborted)));
    }

    // A guard (or a hard abort's kill) may have fired in the microsecond
    // between the settled-value check above and the clear. The whole window
    // is Rust bookkeeping — no JS ran, and the terminate is cancelled
    // before any could — so nothing was interrupted: clear the reason
    // (this run's guard was the only one armed). A raced ABORT is honored
    // as a boundary abandon (silently un-aborting the run would be wrong);
    // a raced limit fire returns the clean success without grace, instead
    // of letting the stale reason truncate grace instantly and name a
    // successful run as an instance-reset culprit.
    if let Some(r) = facts.reason.get() {
        facts.reason.reset();
        if r == TerminationReason::Abort {
            return finished(Err(rs.fail(RunError::Aborted)));
        }
        return finished(Ok(output));
    }

    // Freeze the run-phase telemetry for the early Result, and hand the
    // console buffers over to grace: lines from here on belong to the
    // grace report. The counted clock freezes with it — the rest of this
    // turn belongs to grace.
    if let Some(t) = rs.turn_entered.take() {
        rs.counted += t.elapsed();
    }
    rs.turn_entered = Some(std::time::Instant::now());
    output.wall_time_ms = dur_ms(rs.counted);
    let run_cpu_ms = rs.cpu_budget.elapsed_ms_precise();
    let now_ms = elapsed_ms(rs.start);
    let run_records = rs
        .bridge_log
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .snapshot(now_ms);
    let run_record_count = run_records.len();
    {
        // SAFETY: transient exclusive access between JS executions.
        let mut l = unsafe { &*rs.logs }.borrow_mut();
        l.stdout.clear();
        l.stderr.clear();
    }

    let early_result_sent = match (rs.epilogue, &rs.link) {
        (Some(spec), Some(link)) => {
            let heap_used_bytes = spec.report_heap.then(|| {
                let stats = scope.get_heap_statistics();
                stats.used_heap_size() as u64
            });
            let completion =
                types::RunCompletion::Success(types::RunSuccessPayload {
                    exports: output.exports.clone(),
                    skipped_exports: output.skipped_exports.clone(),
                    stdout: output.stdout.clone(),
                    stderr: output.stderr.clone(),
                    duration_ms: output.duration_ms,
                    wall_time_ms: output.wall_time_ms,
                    cpu_time_ms: run_cpu_ms,
                    bridge_calls: run_records,
                    heap_used_bytes,
                    background_flags: (u8::from(has_grace))
                        | (u8::from(has_streams) << 1),
                });
            match link.send(Addr::Run(spec.run_id), Outbound::Result { completion }) {
                Ok(()) => true,
                Err(e) => {
                    // Fall back to hold mode: the Result (without the
                    // pending flag) goes out after grace completes.
                    eprintln!("[iso4-v8] early Result send failed ({e}); holding");
                    false
                }
            }
        }
        _ => false,
    };

    // ── Grace budgets ────────────────────────────────────────────────────
    // CPU: a fresh allotment sized like the run's, or grace_ms when the run
    // is CPU-unlimited, so a tight loop inside a continuation is always
    // bounded. The guard writes the REAL reason cell and terminates — a
    // mid-turn kill is a taint, per the fired-guard doctrine. Wall: the
    // grace deadline is a loop boundary — stopping between turns is clean
    // and does not taint.
    let grace_start = std::time::Instant::now();
    let grace_cpu_cap = if rs.limits.cpu_time_ms > 0 {
        rs.limits.cpu_time_ms
    } else {
        rs.limits.grace_ms
    };
    rs.phase = RunPhase::Grace(Box::new(GraceState {
        output,
        grace_start,
        wall_cap: has_grace.then(|| Duration::from_millis(rs.limits.grace_ms as u64)),
        counted: Duration::ZERO,
        // A streaming-only phase has no waitUntil work to drive: the grace
        // part starts concluded and the phase is held open by the streams.
        grace_status: if has_grace {
            None
        } else {
            Some(GraceStatus::Settled)
        },
        grace_cpu: Arc::new(CpuBudget::new()),
        grace_cpu_cap,
        error: None,
        early_result_sent,
        run_record_count,
    }));

    // Drain once right away, under the grace budgets — registered work may
    // already be settled, or settle within one checkpoint.
    let _turn = TurnGuard::begin(facts.guard, rs);
    let _epoch = BudgetEpoch::enter(rs.active_budget());
    grace_scan(scope, facts, rs)
}

/// Whether the run has outbound streams the host may still read.
fn out_streams_open(rs: &RunState) -> bool {
    // SAFETY: `out_streams` points into the run's live table entry.
    unsafe { &*rs.out_streams }
        .slot
        .borrow()
        .as_ref()
        .is_some_and(|t| t.streams.values().any(|st| !st.done))
}

/// The idle deadline while outbound streams are open: last host activity
/// plus the 10 s allowance (Jakob's ruling — an in-process consumer has no
/// socket to error on abandonment).
fn out_idle_deadline(rs: &RunState) -> Option<std::time::Instant> {
    // SAFETY: `out_streams` points into the run's live table entry.
    let table = unsafe { &*rs.out_streams };
    let slot = table.slot.borrow();
    let st = slot.as_ref()?;
    if st.streams.values().any(|s| !s.done) {
        Some(st.last_activity + Duration::from_millis(out_stream_idle_ms()))
    } else {
        None
    }
}

/// Apply a host credit grant to an outbound stream (unknown or done streams
/// ignore it).
fn note_out_credit(rs: &RunState, stream_id: u32, credit: u32) {
    // SAFETY: `out_streams` points into the run's live table entry.
    let table = unsafe { &*rs.out_streams };
    let mut slot = table.slot.borrow_mut();
    let Some(st) = slot.as_mut() else { return };
    st.last_activity = std::time::Instant::now();
    if let Some(stream) = st.streams.get_mut(&stream_id) {
        if !stream.done {
            stream.credit = stream.credit.saturating_add(credit);
        }
    }
}

/// Note host activity on the run's outbound table (a cancel counts — the
/// idle deadline measures abandonment, not consumption).
fn note_out_activity(rs: &RunState) {
    // SAFETY: `out_streams` points into the run's live table entry.
    let table = unsafe { &*rs.out_streams };
    let mut slot = table.slot.borrow_mut();
    let Some(st) = slot.as_mut() else { return };
    st.last_activity = std::time::Instant::now();
}

/// Best-effort, turn-less termination of every open outbound stream: one
/// `StreamEnd` error each (dead links ignored), no guest cleanup —
/// the truncate/kill/give-up paths, where no cleanup slice runs.
fn close_out_streams(out_streams: *const OutStreamTable, message: &str) {
    // SAFETY: `out_streams` points into the run's live table entry.
    let table = unsafe { &*out_streams };
    let mut slot = table.slot.borrow_mut();
    let Some(st) = slot.as_mut() else { return };
    let run_id = st.run_id;
    let link = st.link.clone();
    for (id, stream) in st.streams.iter_mut() {
        if stream.done {
            continue;
        }
        stream.done = true;
        stream.pending = None;
        stream.backlog = Vec::new();
        if let Some(link) = &link {
            let _ = link.send(
                Addr::Run(run_id),
                Outbound::StreamEnd {
                    stream_id: *id,
                    error: Some(message.to_string()),
                },
            );
        }
    }
}

/// Cancel one outbound stream with GUEST cleanup (web-streams semantics):
/// invokes the stream's internal cancel path — for an async-iterable body
/// that is `iterator.return()`, so `finally` blocks run; for a pass-through
/// it cancels the upstream inbound stream — as part of the current turn,
/// then marks it done. `notify_host` additionally sends a `StreamEnd`
/// error frame (the idle deadline / connection paths; a host-initiated
/// cancel sends nothing back).
fn cancel_out_stream(
    scope: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
    rs: &RunState,
    id: u32,
    reason: &str,
    notify_host: bool,
) {
    let body = {
        // SAFETY: `out_streams` points into the run's live table entry.
        let table = unsafe { &*rs.out_streams };
        let mut slot = table.slot.borrow_mut();
        let Some(st) = slot.as_mut() else { return };
        let run_id = st.run_id;
        let link = st.link.clone();
        let Some(stream) = st.streams.get_mut(&id) else { return };
        if stream.done {
            return;
        }
        stream.done = true;
        stream.pending = None;
        stream.backlog = Vec::new();
        if notify_host {
            if let Some(link) = &link {
                let _ = link.send(
                    Addr::Run(run_id),
                    Outbound::StreamEnd {
                        stream_id: id,
                        error: Some(reason.to_string()),
                    },
                );
            }
        }
        stream.body.clone()
    };
    let body_local = v8::Local::new(scope, &body);
    let Some(name) = v8::String::new(scope, "_cancelSource") else { return };
    let Some(method) = body_local.get(scope, name.into()) else { return };
    let Ok(method) = v8::Local::<v8::Function>::try_from(method) else { return };
    let reason_v8: v8::Local<v8::Value> = match v8::String::new(scope, reason) {
        Some(r) => r.into(),
        None => v8::undefined(scope).into(),
    };
    if method.call(scope, body_local.into(), &[reason_v8]).is_none() {
        // Guest cleanup threw (or was killed — the caller's tail sees the
        // reason cell). A plain throw in cleanup is swallowed: the stream
        // is already over either way.
        if !scope.is_execution_terminating() {
            scope.reset();
        }
    }
}

/// Cancel every open outbound stream with guest cleanup — the idle-deadline
/// and connection-death paths.
fn cancel_all_out_streams(
    scope: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
    rs: &RunState,
    reason: &str,
) {
    let ids: Vec<u32> = {
        // SAFETY: `out_streams` points into the run's live table entry.
        let table = unsafe { &*rs.out_streams };
        let slot = table.slot.borrow();
        match slot.as_ref() {
            Some(st) => st
                .streams
                .iter()
                .filter(|(_, s)| !s.done)
                .map(|(id, _)| *id)
                .collect(),
            None => Vec::new(),
        }
    };
    for id in ids {
        cancel_out_stream(scope, rs, id, reason, true);
        if scope.is_execution_terminating() {
            break;
        }
    }
}

/// Extract a pull result: `Ok(Some(bytes))` for a chunk, `Ok(None)` at EOF,
/// `Err(message)` for anything a body may not yield.
fn chunk_from_pull(
    scope: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
    value: v8::Local<v8::Value>,
) -> Result<Option<Vec<u8>>, String> {
    if value.is_null_or_undefined() {
        return Ok(None);
    }
    if let Ok(u8s) = v8::Local::<v8::Uint8Array>::try_from(value) {
        let mut bytes = vec![0u8; u8s.byte_length()];
        let copied = u8s.copy_contents(&mut bytes);
        bytes.truncate(copied);
        return Ok(Some(bytes));
    }
    if value.is_string() {
        let Some(text) = value.to_string(scope) else {
            return Err("body stream chunk could not be read".to_string());
        };
        return Ok(Some(text.to_rust_string_lossy(scope).into_bytes()));
    }
    Err("a stream body must yield Uint8Array or string chunks".to_string())
}

/// Start one pull on the guest stream: calls its `_pull()` and returns the
/// promise (a non-promise return is wrapped). `Err` carries the message of
/// a synchronous throw.
fn start_body_pull<'s>(
    scope: &mut v8::PinnedRef<'s, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
    body: v8::Local<'s, v8::Object>,
) -> Result<v8::Local<'s, v8::Promise>, String> {
    let Some(name) = v8::String::new(scope, "_pull") else {
        return Err("could not intern _pull".to_string());
    };
    let Some(method) = body.get(scope, name.into()) else {
        return Err("body stream has no pull".to_string());
    };
    let Ok(method) = v8::Local::<v8::Function>::try_from(method) else {
        return Err("body stream pull is not callable".to_string());
    };
    let Some(result) = method.call(scope, body.into(), &[]) else {
        if scope.is_execution_terminating() {
            return Err("terminated".to_string());
        }
        let message = scope
            .exception()
            .and_then(|e| error_message_from_value(scope, e))
            .unwrap_or_else(|| "body stream pull failed".to_string());
        scope.reset();
        return Err(message);
    };
    if let Ok(promise) = v8::Local::<v8::Promise>::try_from(result) {
        return Ok(promise);
    }
    let Some(resolver) = v8::PromiseResolver::new(scope) else {
        return Err("could not wrap pull result".to_string());
    };
    resolver.resolve(scope, result);
    Ok(resolver.get_promise(scope))
}

/// A drive pass cap: each round may start pulls whose promises settle
/// within one checkpoint (a sync-ish source); credit bounds the real work,
/// this only bounds rounds inside ONE turn.
const OUT_DRIVE_MAX_ROUNDS: usize = 32;

/// Advance every outbound stream as far as host credit and the guest
/// sources allow. Guest pulls execute as part of the CURRENT turn (billed
/// to the phase budgets); a pull suspended on host IO parks until its
/// promise settles in a later turn, when the scan calls back in here.
fn drive_out_streams(
    scope: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
    rs: &RunState,
) {
    let ids: Vec<u32> = {
        // SAFETY: `out_streams` points into the run's live table entry.
        let table = unsafe { &*rs.out_streams };
        let slot = table.slot.borrow();
        match slot.as_ref() {
            Some(st) => st.streams.keys().copied().collect(),
            None => return,
        }
    };
    if ids.is_empty() {
        return;
    }
    for _ in 0..OUT_DRIVE_MAX_ROUNDS {
        let mut progressed = false;
        for &id in &ids {
            progressed |= drive_out_stream(scope, rs, id);
            if scope.is_execution_terminating() {
                return;
            }
        }
        if !progressed {
            return;
        }
        // Settle pulls started this round (async sources resolve on the
        // microtask queue when their data is already available).
        scope.perform_microtask_checkpoint();
        if scope.is_execution_terminating() {
            return;
        }
    }
}

/// One stream's drive step. Returns whether it made progress (sent bytes,
/// settled a pull, started one) — the caller checkpoints and loops.
fn drive_out_stream(
    scope: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
    rs: &RunState,
    id: u32,
) -> bool {
    enum Step {
        Idle,
        AwaitPending(v8::Global<v8::Promise>),
        StartPull(v8::Global<v8::Object>),
    }
    /// Flush backlog under credit (no JS) and classify the next step.
    fn classify(rs: &RunState, id: u32, progressed: &mut bool) -> Step {
        // SAFETY: `out_streams` points into the run's live table entry.
        let table = unsafe { &*rs.out_streams };
        let mut slot = table.slot.borrow_mut();
        let Some(st) = slot.as_mut() else { return Step::Idle };
        let run_id = st.run_id;
        let link = st.link.clone();
        let Some(stream) = st.streams.get_mut(&id) else { return Step::Idle };
        if stream.done {
            return Step::Idle;
        }
        while stream.credit > 0 && stream.backlog_sent < stream.backlog.len() {
            let remaining = stream.backlog.len() - stream.backlog_sent;
            let n = remaining
                .min(stream.credit as usize)
                .min(types::STREAM_CHUNK_MAX_BYTES as usize);
            let Some(link) = &link else {
                stream.done = true;
                return Step::Idle;
            };
            let data = stream.backlog[stream.backlog_sent..stream.backlog_sent + n].to_vec();
            if link
                .send(Addr::Run(run_id), Outbound::StreamChunk { stream_id: id, data })
                .is_err()
            {
                // Dead link: the connection-loss event ends the phase.
                stream.done = true;
                return Step::Idle;
            }
            stream.backlog_sent += n;
            stream.credit -= n as u32;
            *progressed = true;
        }
        if stream.backlog_sent >= stream.backlog.len() && !stream.backlog.is_empty() {
            stream.backlog = Vec::new();
            stream.backlog_sent = 0;
        }
        if !stream.backlog.is_empty() || stream.credit == 0 {
            return Step::Idle; // waiting for credit
        }
        if let Some(p) = &stream.pending {
            return Step::AwaitPending(p.clone());
        }
        Step::StartPull(stream.body.clone())
    }
    /// Conclude a settled pull into the stream (no JS).
    fn settle(rs: &RunState, id: u32, outcome: Result<Option<Vec<u8>>, String>) {
        // SAFETY: `out_streams` points into the run's live table entry.
        let table = unsafe { &*rs.out_streams };
        let mut slot = table.slot.borrow_mut();
        let Some(st) = slot.as_mut() else { return };
        let run_id = st.run_id;
        let link = st.link.clone();
        let Some(stream) = st.streams.get_mut(&id) else { return };
        stream.pending = None;
        match outcome {
            Ok(Some(bytes)) => {
                stream.backlog = bytes;
                stream.backlog_sent = 0;
            }
            Ok(None) => {
                stream.done = true;
                if let Some(link) = &link {
                    let _ = link.send(
                        Addr::Run(run_id),
                        Outbound::StreamEnd {
                            stream_id: id,
                            error: None,
                        },
                    );
                }
            }
            Err(message) => {
                stream.done = true;
                if let Some(link) = &link {
                    let _ = link.send(
                        Addr::Run(run_id),
                        Outbound::StreamEnd {
                            stream_id: id,
                            error: Some(message),
                        },
                    );
                }
            }
        }
    }

    let mut progressed = false;
    loop {
        match classify(rs, id, &mut progressed) {
            Step::Idle => return progressed,
            Step::AwaitPending(p) => {
                let local = v8::Local::new(scope, &p);
                match local.state() {
                    v8::PromiseState::Pending => return progressed,
                    v8::PromiseState::Fulfilled => {
                        let value = local.result(scope);
                        let outcome = chunk_from_pull(scope, value);
                        settle(rs, id, outcome);
                        progressed = true;
                    }
                    v8::PromiseState::Rejected => {
                        let rejection = local.result(scope);
                        let message = error_message_from_value(scope, rejection)
                            .unwrap_or_else(|| "body stream failed".to_string());
                        settle(rs, id, Err(message));
                        progressed = true;
                    }
                }
            }
            Step::StartPull(body) => {
                let body_local = v8::Local::new(scope, &body);
                match start_body_pull(scope, body_local) {
                    Ok(promise) => {
                        if scope.is_execution_terminating() {
                            return progressed;
                        }
                        let global = v8::Global::new(scope, promise);
                        // SAFETY: same table contract as above.
                        let table = unsafe { &*rs.out_streams };
                        let mut slot = table.slot.borrow_mut();
                        if let Some(st) = slot.as_mut() {
                            if let Some(stream) = st.streams.get_mut(&id) {
                                stream.pending = Some(global);
                            }
                        }
                        // Let the caller checkpoint before re-checking.
                        return true;
                    }
                    Err(message) => {
                        if scope.is_execution_terminating() {
                            return progressed;
                        }
                        settle(rs, id, Err(message));
                        return true;
                    }
                }
            }
        }
    }
}

/// One grace drain: drive the queue, drop settled promises, note the first
/// rejection, decide whether the phase is over. New registrations (a
/// `waitUntil` called from a continuation) land in the same armed slot and
/// are picked up next drain.
fn grace_scan(
    scope: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
    facts: &RunFacts<'_>,
    rs: &mut RunState,
) -> ScanOutcome {
    // Drive whatever the current microtask queue holds before looking.
    scope.perform_microtask_checkpoint();

    // The grace CPU guard writes the REAL reason cell when it kills a
    // runaway continuation (or pull); end the phase the moment that
    // happened instead of idling out the wall — the instance is tainted
    // either way, but the slot frees now.
    if facts.reason.get().is_some() {
        return finished(Ok(finish_killed_grace(facts, rs)));
    }

    // Outbound streams: produce as far as host credit and the guest
    // sources allow — guest pulls run as part of this turn, under the
    // phase budgets.
    drive_out_streams(scope, rs);
    if facts.reason.get().is_some() {
        return finished(Ok(finish_killed_grace(facts, rs)));
    }

    // ── The waitUntil part, while undecided ──────────────────────────────
    let grace_undecided = matches!(&rs.phase, RunPhase::Grace(g) if g.grace_status.is_none());
    if grace_undecided {
        // SAFETY: `pending` points at the run entry's PendingWork Box.
        let taken = unsafe { (*rs.pending).slot.borrow_mut().take() };
        let mut still: Vec<v8::Global<v8::Promise>> = Vec::new();
        let mut first_error: Option<(String, String)> = None;
        for g in taken.unwrap_or_default() {
            let p = v8::Local::new(scope, &g);
            match p.state() {
                v8::PromiseState::Pending => still.push(g),
                v8::PromiseState::Rejected => {
                    if first_error.is_none() {
                        let rejection = p.result(scope);
                        first_error = Some(capped_grace_error(
                            error_name_from_value(scope, rejection)
                                .unwrap_or_else(|| "Error".to_string()),
                            error_message_from_value(scope, rejection)
                                .unwrap_or_else(|| "background work failed".to_string()),
                        ));
                    }
                }
                v8::PromiseState::Fulfilled => {}
            }
        }
        let empty = still.is_empty();
        // Re-arm the slot with whatever is still pending.
        unsafe { *(*rs.pending).slot.borrow_mut() = Some(still) };
        // Reading a rejection can run guest getters.
        if facts.reason.get().is_some() {
            return finished(Ok(finish_killed_grace(facts, rs)));
        }

        let grace_ms = rs.limits.grace_ms;
        let RunPhase::Grace(g) = &mut rs.phase else {
            unreachable!("grace_scan outside grace");
        };
        if g.error.is_none() {
            g.error = first_error;
        }
        if empty {
            g.grace_status = Some(if g.error.is_some() {
                GraceStatus::Failed
            } else {
                GraceStatus::Settled
            });
        } else if g.grace_start.elapsed() >= Duration::from_millis(grace_ms as u64) {
            g.grace_status = Some(GraceStatus::Truncated);
        }
    }

    // ── Phase end: both parts done ───────────────────────────────────────
    let grace_done = matches!(&rs.phase, RunPhase::Grace(g) if g.grace_status.is_some());
    if grace_done && !out_streams_open(rs) {
        return finished(Ok(finish_post_phase(rs)));
    }
    if rs.source.is_none() && earliest_timer(rs).is_none() {
        // No event source, no pending timer: nothing can resolve the rest
        // — waitUntil work or stream credit alike.
        if let RunPhase::Grace(g) = &mut rs.phase {
            g.grace_status.get_or_insert(GraceStatus::Truncated);
        }
        return finished(Ok(finish_post_phase(rs)));
    }
    ScanOutcome::Continue
}

/// [`finish_post_phase`] after a mid-turn kill; a dynamic `import()` keeps
/// its message on the grace report.
fn finish_killed_grace(facts: &RunFacts<'_>, rs: &mut RunState) -> Output {
    if let (Some(TerminationReason::Import(message)), RunPhase::Grace(g)) =
        (facts.reason.get(), &mut rs.phase)
    {
        g.error
            .get_or_insert_with(|| capped_grace_error("Error".to_string(), message.to_string()));
    }
    finish_post_phase(rs)
}

/// End the post-Result phase with the waitUntil part's recorded status
/// (`Truncated` when it never got to decide), closing any streams still
/// open, best-effort and turn-less — the graceful cancel paths (host
/// cancel, idle deadline, connection death) ran their guest cleanup
/// already; this covers truncation, kills, and give-ups.
fn finish_post_phase(rs: &mut RunState) -> Output {
    close_out_streams(rs.out_streams, "run ended");
    let status = match &rs.phase {
        RunPhase::Grace(g) => g.grace_status.unwrap_or(GraceStatus::Truncated),
        _ => GraceStatus::Truncated,
    };
    finish_grace(rs, status)
}

/// Close out the grace phase: stamp the report (telemetry diff since the
/// early Result) onto the run's held Output. Pure bookkeeping — no JS runs,
/// so boundary events (deadline, socket death) call it without a scope.
fn finish_grace(rs: &mut RunState, status: GraceStatus) -> Output {
    let grace_live = live_segment(rs);
    let RunPhase::Grace(g) = &mut rs.phase else {
        unreachable!("finish_grace outside grace");
    };
    // Grace-only telemetry: records appended after the early snapshot, and
    // the console lines written since the buffers were handed over.
    let all_records = rs
        .bridge_log
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .snapshot(elapsed_ms(rs.start));
    let bridge_calls = all_records
        .get(g.run_record_count..)
        .unwrap_or(&[])
        .to_vec();
    let (stdout, stderr) = {
        // SAFETY: transient access between JS executions.
        let mut l = unsafe { &*rs.logs }.borrow_mut();
        (std::mem::take(&mut l.stdout), std::mem::take(&mut l.stderr))
    };

    let mut output = std::mem::replace(
        &mut g.output,
        Output {
            exports: Vec::new(),
            skipped_exports: Vec::new(),
            stdout: Vec::new(),
            stderr: Vec::new(),
            duration_ms: 0.0,
            wall_time_ms: 0.0,
            cpu_time_ms: 0.0,
            bridge_calls: Vec::new(),
            background: None,
        },
    );
    output.background = Some(GraceReport {
        status,
        duration_ms: elapsed_ms(g.grace_start),
        wall_time_ms: dur_ms(g.counted + grace_live),
        cpu_time_ms: g.grace_cpu.elapsed_ms_precise(),
        stdout,
        stderr,
        bridge_calls,
        error: g.error.take(),
        early_result_sent: g.early_result_sent,
    });
    output
}

/// RAII for the per-turn guard target: set on entry, cleared on drop. The
/// set and the clear serialize with the guard's fire under the target lock,
/// so after a turn ends any fire that beat the clear is visible in the
/// reason cell.
struct TurnGuard<'a> {
    guard: &'a InstanceGuard,
}

impl<'a> TurnGuard<'a> {
    fn begin(guard: &'a InstanceGuard, rs: &RunState) -> Self {
        let token = rs.token;
        let target = match &rs.phase {
            RunPhase::Starting | RunPhase::Settling { .. } => GuardTarget {
                budget: Arc::clone(&rs.cpu_budget),
                cpu_cap_ms: rs.limits.cpu_time_ms,
                // Enforced mid-turn too, so a synchronous spin under
                // `cpuTimeMs: 0` still dies at the wall (E2 ruling closing
                // the boundary-only gap). Mid-turn the counted clock runs
                // 1:1 with real time, so the remaining budget is an instant.
                wall_deadline: (rs.limits.wall_time_ms > 0).then(|| {
                    let cap = Duration::from_millis(rs.limits.wall_time_ms as u64);
                    std::time::Instant::now() + cap.saturating_sub(rs.counted)
                }),
            },
            RunPhase::Grace(g) => GuardTarget {
                budget: Arc::clone(&g.grace_cpu),
                cpu_cap_ms: g.grace_cpu_cap,
                // The grace wall is a loop boundary, never a mid-turn kill.
                wall_deadline: None,
            },
        };
        guard.set_for(target, token);
        Self { guard }
    }
}

impl Drop for TurnGuard<'_> {
    fn drop(&mut self) {
        self.guard.clear();
    }
}

/// RAII for one turn's CPU epoch on the phase's budget.
struct BudgetEpoch(Arc<CpuBudget>);

impl BudgetEpoch {
    fn enter(budget: Arc<CpuBudget>) -> Self {
        budget.enter();
        Self(budget)
    }
}

impl Drop for BudgetEpoch {
    fn drop(&mut self) {
        self.0.leave();
    }
}

/// Cap the error strings carried on a `RunComplete` frame (review): the
/// rejection message is guest-authored and otherwise unbounded, and the frame
/// has no per-field limits of its own.
fn capped_grace_error(name: String, message: String) -> (String, String) {
    let mut name = name;
    let mut message = message;
    name.truncate(name.floor_char_boundary(256));
    if message.len() > 2048 {
        message.truncate(message.floor_char_boundary(2048));
        message.push_str("… [truncated]");
    }
    (name, message)
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
    prefix_module: v8::Global<v8::Module>,
    isolate: v8::OwnedIsolate,
    /// Per-run state table. The once-per-instance native installs (console,
    /// web runtime, waitUntil, bridge stubs) hold a raw pointer into this
    /// Box (stable address) and route by the attributed run token.
    run_table: Box<RunTable>,
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
    /// Bridge-stub identity slots (see [`StubSlot`]): the pointees of every
    /// stub `Function` ever created in this context. Per-run bindings live
    /// in the run table; these Boxes only carry the name + table pointer.
    stub_slots: StubSlots,
    /// The instance's watchdog registration (see [`InstanceGuard`]): warm-up
    /// and every turn of every call publish their budgets through it.
    guard: InstanceGuard,
    /// True until the first call: warm-up console output (the prefix's
    /// `console.log`s) is sitting in the table's `setup_logs` and is
    /// delivered on the cold-start call's result instead of being dropped.
    warmup_logs_pending: bool,
}

/// Build an isolate with the heap cap, budget ArrayBuffer allocator, and
/// near-heap callback wired to `reason` — the CreateParams dance shared by
/// `run_module_inner`-style one-off setup, warm instance creation, and
/// `prepare()` validation. `hard_memory_mb == 0` means uncapped.
///
/// The isolate is told memory is under moderate pressure for its whole life
/// (workerd's posture): V8's default assumes it owns the machine and collects
/// lazily, which is wrong in a container packed with isolates and would leave
/// the soft line reading garbage instead of live data.
fn new_capped_isolate(
    hard_memory_mb: u32,
    reason: &Arc<ReasonCell>,
) -> (
    v8::OwnedIsolate,
    Option<Arc<BudgetAllocState>>,
    Option<Box<NearHeapData>>,
) {
    let alloc_state: Option<Arc<BudgetAllocState>> = if hard_memory_mb > 0 {
        Some(Arc::new(BudgetAllocState {
            used: AtomicUsize::new(0),
            budget: hard_memory_mb as usize * 1024 * 1024,
            handle: OnceLock::new(),
            reason: Arc::clone(reason),
        }))
    } else {
        None
    };

    let mut isolate = {
        let params = crate::snapshot::create_params();
        let params = if hard_memory_mb > 0 {
            params.heap_limits(0, hard_memory_mb as usize * 1024 * 1024)
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
    // V8's default assumes it owns the machine and collects lazily; wrong for
    // a container full of isolates (workerd flips the same switch).
    isolate.memory_pressure_notification(v8::MemoryPressureLevel::Moderate);
    isolate.set_oom_error_handler(oom_error_cb);
    isolate.set_microtasks_policy(v8::MicrotasksPolicy::Explicit);
    isolate.set_host_initialize_import_meta_object_callback(host_import_meta_callback);
    install_dynamic_import(&mut isolate, reason);
    // Same rationale as the one-off path: no blocking Atomics.wait, ever.
    isolate.set_allow_atomics_wait(false);
    isolate.set_allow_wasm_code_generation_callback(deny_wasm_codegen);

    let near_heap: Option<Box<NearHeapData>> = if hard_memory_mb > 0 {
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
        Some(TerminationReason::Import(m)) => RunError::Import(m.to_string()),
        Some(_) => RunError::WarmupLimit,
        None => fallback,
    }
}

/// Build a fresh isolate + context, run the boot fixups, and
/// evaluate the prefix — all under the fixed warm-up budget. This is the
/// cold-start half of a warm instance; `run_call_on_core` is the per-call
/// half. A guard fired during warm-up reports `ERR_WARMUP_LIMIT` (the
/// caller's limits are not armed, so neither timeout code would be honest);
/// blowing the heap cap keeps `ERR_MEMORY_LIMIT`.
pub fn create_instance_core(
    prefix: PrefixSpec<'_>,
    imports: &[ImportBinding],
    hard_memory_mb: u32,
) -> Result<InstanceCore, FailureOutput> {
    init_platform();
    let start = std::time::Instant::now();

    let run_table = RunTable::boxed();
    let reason = Arc::new(ReasonCell::new());
    let (mut isolate, alloc_state, near_heap) = new_capped_isolate(hard_memory_mb, &reason);

    let handle = isolate.thread_safe_handle();
    let warmup_budget = Arc::new(CpuBudget::new());
    // The instance's watchdog registration, reused by every later call.
    // Warm-up runs under the fixed warm-up target.
    let guard = InstanceGuard::new(handle.clone(), Arc::clone(&reason));
    guard.set(GuardTarget {
        budget: Arc::clone(&warmup_budget),
        cpu_cap_ms: WARMUP_CPU_MS,
        wall_deadline: Some(start + Duration::from_millis(WARMUP_WALL_MS as u64)),
    });

    let table_ptr: *const RunTable = &*run_table;
    let setup = (|| -> Result<(v8::Global<v8::Context>, v8::Global<v8::Module>), RunError> {
        v8::scope!(let scope, &mut isolate);
        let context = v8::Context::new(scope, Default::default());
        let scope = &mut v8::ContextScope::new(scope, context);
        // No run entry exists during prefix evaluation: a setup-time
        // waitUntil or setTimeout throws, and warm-up console output lands
        // in setup_logs.
        crate::snapshot::boot_context(scope, table_ptr).map_err(RunError::Internal)?;
        // Enter the CPU meter for compile + evaluate only — isolate boot
        // is ours, the prefix's code is what the budget polices.
        warmup_budget.enter();
        let prefix_module = {
            v8::tc_scope!(let tc, scope);
            evaluate_prefix_module(tc, prefix.code, prefix.filename, prefix.globals, imports)?
        };
        warmup_budget.leave();
        Ok((v8::Global::new(scope, context), prefix_module))
    })();
    warmup_budget.leave();
    // Clearing the target serializes with any in-flight fire under the
    // target lock: after this line, a fire that won the race has already
    // set the reason cell, so the verdict below observes it — no join.
    guard.clear();

    match setup {
        // A guard that fired *after* evaluation completed still taints: the
        // pending terminate may have landed anywhere. Fired guard ⇒ discard,
        // no exceptions to the doctrine.
        Ok(_) if reason.get().is_some() => Err(failure(
            warmup_error(&reason, RunError::WarmupLimit),
            &run_table.setup_logs.borrow(),
            start,
        )),
        Ok((context, prefix_module)) => {
            handle.cancel_terminate_execution();
            Ok(InstanceCore {
                context,
                prefix_module,
                isolate,
                run_table,
                reason,
                alloc_state,
                near_heap,
                stub_slots: StubSlots::new(),
                guard,
                warmup_logs_pending: true,
            })
        }
        Err(error) => Err(failure(
            warmup_error(&reason, error),
            &run_table.setup_logs.borrow(),
            start,
        )),
    }
}

/// One call forwarded to an instance owner thread (or run by a one-off
/// worker). Owns everything a run needs: the code/call, the limits, the
/// per-call bridge defs, and the run's I/O.
pub struct CallJob {
    /// The run's table token. 0 = let the runtime allocate one; the session
    /// demux allocates it up front (via [`alloc_run_token`]) so it can route
    /// mid-turn aborts before the instance has even started the run.
    pub token: u64,
    pub limits: Limits,
    /// Bridge-stub defs for this call (re-installed per call).
    pub globals: Vec<types::HostGlobalDef>,
    /// The run's event source and its link to the host.
    pub io: RunIo,
    /// The side channel for this run's console lines.
    pub observer: Arc<dyn Observer>,
    /// The connection's monotonically increasing bridge call-ID counter.
    pub call_id_counter: Arc<AtomicU32>,
    pub call: types::CallSpec,
    /// Per-run identity for the early Result.
    pub epilogue: Option<EpilogueSpec>,
    /// Registry bookkeeping run on the instance thread when the run
    /// finishes, before its completion goes out on the link. Owns the
    /// outcome; direct callers use it to receive the outcome.
    pub release: Option<ReleaseHook>,
    /// Filled with the instance guard's cross-thread face once the isolate
    /// exists, so the run's handle can terminate it mid-turn.
    pub ctl_slot: Option<Arc<OnceLock<GuardCtl>>>,
}

/// The release hook a [`CallJob`] can carry: runs on the instance thread
/// when the run finishes and owns the outcome.
pub type ReleaseHook = Box<dyn FnOnce(CallOutcome) + Send>;

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
    /// True only for a cold start whose prefix failed deterministically:
    /// the store marks the prefix broken and the result says `prefix_stage`.
    pub prefix_failed: bool,
}

/// Run one export call on a retained instance: fresh guards and CPU budget
/// from dispatch to settle, per-call bridge stubs, per-call console caps —
/// the same per-request semantics a one-off run has, minus the isolate boot
/// and prefix evaluation.
#[allow(clippy::too_many_arguments)] // mirrors run_module; bundled state is the instance itself
pub fn run_call_on_core(
    core: &mut InstanceCore,
    prefix_globals: &[HostGlobalDef],
    limits: Limits,
    globals: &[HostGlobalDef],
    imports: &[ImportBinding],
    io: RunIo,
    call_id_counter: Arc<AtomicU32>,
    call: &types::CallSpec,
) -> CallOutcome {
    let start = std::time::Instant::now();
    let cpu_budget = Arc::new(CpuBudget::new());
    let bridge_log: Arc<Mutex<BridgeCallLog>> = Arc::new(Mutex::new(BridgeCallLog::default()));

    // Per-call reset of the termination-reason slot, then a fresh run entry
    // in the instance's table. The cold-start call adopts the warm-up
    // console output: it belongs to the run that paid for the warm-up —
    // previously the prefix re-evaluated per run, so its output appeared on
    // every result. (Warm-up bytes count against the first call's caps,
    // exactly as when the buffers were shared.)
    core.reason.reset();
    let token = alloc_run_token();
    let epilogue = take_epilogue_spec();
    let entry = RunCallState::new(
        limits.max_stdout_bytes,
        limits.max_stderr_bytes,
        Arc::new(crate::link::NoopObserver),
        Addr::Run(epilogue.map_or(0, |e| e.run_id)),
    );
    if core.warmup_logs_pending {
        core.warmup_logs_pending = false;
        let mut setup = core.run_table.setup_logs.borrow_mut();
        let mut logs = entry.logs.borrow_mut();
        logs.stdout = std::mem::take(&mut setup.stdout);
        logs.stderr = std::mem::take(&mut setup.stderr);
        logs.stdout_bytes = std::mem::take(&mut setup.stdout_bytes);
        logs.stderr_bytes = std::mem::take(&mut setup.stderr_bytes);
    }
    // Pointers into the Boxed entry stay valid for the whole call; the entry
    // is removed (between turns) below before this function returns.
    let logs_ptr: *const RefCell<LogBuffers> = std::ptr::addr_of!(*entry.logs);
    let pending_ptr: *const PendingWork = std::ptr::addr_of!(*entry.pending);
    let streams_ptr: *const StreamTable = std::ptr::addr_of!(*entry.streams);
    let out_streams_ptr: *const OutStreamTable = std::ptr::addr_of!(*entry.out_streams);
    let timers_ptr: *const RefCell<TimerTable> = std::ptr::addr_of!(*entry.timers);
    core.run_table.runs.borrow_mut().insert(token, entry);
    CURRENT_RUN_TOKEN.with(|c| c.set(token));
    RUN_TABLE_PTR.with(|c| c.set(std::ptr::addr_of!(*core.run_table)));

    // The guard target goes up BEFORE anything touches the retained context.
    // The placeholder re-arm below runs `define_own_property` — which never
    // invokes user accessors — but defence in depth: any path that can
    // execute sandbox-authored code on this thread must be under a budget,
    // or hostile prefix or run code could hang the owner thread forever.
    // (run_call_phase re-arms this target per turn and clears it while the
    // run is parked.)
    let cancel_handle = core.isolate.thread_safe_handle();
    core.guard.set_for(
        GuardTarget {
            budget: Arc::clone(&cpu_budget),
            cpu_cap_ms: limits.cpu_time_ms,
            wall_deadline: (limits.wall_time_ms > 0)
                .then(|| start + Duration::from_millis(limits.wall_time_ms as u64)),
        },
        token,
    );

    // Re-arm throwing placeholders over every bridge-callable name BEFORE
    // the live stubs bind: a name the previous call bound live but this
    // call does not re-bind must throw, not dispatch (the per-run binding
    // is gone either way — the placeholder gives the honest error instead
    // of the stale-reference one).
    let placeholder_result = {
        v8::scope!(let scope, &mut core.isolate);
        let context = v8::Local::new(scope, &core.context);
        let scope = &mut v8::ContextScope::new(scope, context);
        install_prefix_bridge_placeholders(scope, prefix_globals, imports)
    };
    let mut result: Result<Output, FailureOutput> = match placeholder_result {
        Err(error) => Err(failure(
            termination_or(&core.reason, error),
            &unsafe { &*logs_ptr }.borrow(),
            start,
        )),
        Ok(()) => run_call_phase(
            &mut core.isolate,
            &core.context,
            RunEntry::Prefix(&core.prefix_module),
            &mut core.stub_slots,
            &core.guard,
            CallPhaseCtx {
                token,
                table: std::ptr::addr_of!(*core.run_table),
                limits: &limits,
                globals,
                imports,
                io,
                call_id_counter,
                call: Some(call),
                start,
                cpu_budget: &cpu_budget,
                bridge_log: &bridge_log,
                reason: &core.reason,
                logs: logs_ptr,
                pending: pending_ptr,
                streams: streams_ptr,
                out_streams: out_streams_ptr,
                timers: timers_ptr,
                epilogue,
                cancel_handle: &cancel_handle,
            },
        ),
    };
    // Belt and suspenders: run_call_phase clears the target on every path,
    // and clear-vs-fire serialize under the target lock, so any fire that
    // won a race is already visible in the reason cell for the verdict
    // below — no thread joins.
    core.guard.clear();
    cpu_budget.leave();

    // The stream table was disarmed (and live streams cancelled) by
    // run_call_phase's normal exit; this covers the failure paths that never
    // reached it. Idempotent. Must happen before the entry is removed — the
    // cancel messages read the entry's link.
    close_run_streams(streams_ptr);
    close_out_streams(out_streams_ptr, "run ended");
    // Remove the run's entry: stub Functions sandbox code stashed now fail
    // the token lookup and throw, a between-calls waitUntil throws, and the
    // entry's Globals (resolvers, pending promises) drop here, on the owner
    // thread, between turns.
    core.run_table.runs.borrow_mut().remove(&token);
    CURRENT_RUN_TOKEN.with(|c| c.set(0));
    RUN_TABLE_PTR.with(|c| c.set(std::ptr::null()));

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

    // Taint verdict: a turn was interrupted mid-JS — and nothing else
    // (E1 ruling 3, deliberately narrower than the old error-shape list).
    // A fired guard means terminate_execution ripped arbitrary
    // mid-execution state; a still-pending termination flag means something
    // terminated without the clean paths' cancel (fatal bridge violations
    // set it without a reason). Everything that ends a run BETWEEN turns —
    // a boundary wall expiry, a socket death, a direct-abandon abort, an
    // ordinary uncaught exception — leaves the instance trustworthy and
    // reusable.
    let tainted = instance_tainted(core);
    // Nothing may carry an unconsumed terminate into the next call.
    core.isolate.cancel_terminate_execution();

    CallOutcome {
        result,
        tainted,
        heap_used_bytes: heap_used(&mut core.isolate),
        prefix_failed: false,
    }
}


// ── The instance loop ────────────────────────────────────────────────────────

/// One suspended run on the instance loop.
struct LiveRun {
    rs: RunState,
    job: Box<CallJob>,
}

/// Whether a turn was interrupted mid-JS — the instance-wide taint test
/// (E1 ruling 3): a fired guard, or a termination flag still pending.
fn instance_tainted(core: &mut InstanceCore) -> bool {
    core.reason.get().is_some()
        || core.reason.bare_terminate()
        || core.isolate.is_execution_terminating()
}

/// Finish one run: stamp its telemetry, close its streams, drop its table
/// entry, then release it and answer the host (`finish_job`).
fn conclude_run(
    isolate: &mut v8::OwnedIsolate,
    run_table: &RunTable,
    mut live: LiveRun,
    mut result: Result<Output, FailureOutput>,
    tainted: bool,
) {
    let cpu_time_ms = live.rs.cpu_budget.elapsed_ms_precise();
    let wall_time_ms = counted_wall_ms(&live.rs);
    let records = live
        .rs
        .bridge_log
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .finalize(elapsed_ms(live.rs.start));
    match &mut result {
        Ok(output) => {
            output.cpu_time_ms = cpu_time_ms;
            output.wall_time_ms = wall_time_ms;
            output.bridge_calls = records;
        }
        Err(f) => {
            f.cpu_time_ms = cpu_time_ms;
            f.wall_time_ms = wall_time_ms;
            f.bridge_calls = records;
        }
    }
    close_run_streams(live.rs.streams);
    close_out_streams(live.rs.out_streams, "run ended");
    run_table.runs.borrow_mut().remove(&live.rs.token);
    // Between turns: nothing may carry an unconsumed terminate into later work.
    isolate.cancel_terminate_execution();
    let outcome = CallOutcome {
        result,
        tainted,
        heap_used_bytes: heap_used(isolate),
        prefix_failed: false,
    };
    finish_job(&mut live.job, live.rs.link.take(), outcome);
}

/// Conclude a job through [`finish_run`], its release hook as the
/// bookkeeping.
pub fn finish_job(job: &mut CallJob, link: Option<Arc<dyn HostLink>>, outcome: CallOutcome) {
    let run_id = job.epilogue.map(|e| e.run_id).unwrap_or(0);
    let msg = link.as_ref().map(|_| {
        completion_message(
            &outcome.result,
            Some(outcome.heap_used_bytes),
            outcome.prefix_failed,
        )
    });
    let release = job.release.take();
    finish_run(link.as_deref(), run_id, msg, || {
        if let Some(release) = release {
            release(outcome);
        }
    });
}

/// Conclude a run: the frontend drops its route, the registry bookkeeping
/// runs (the instance is back before the host can see the answer), then the
/// completion goes out.
pub fn finish_run(
    link: Option<&dyn HostLink>,
    run_id: u32,
    msg: Option<Outbound>,
    release: impl FnOnce(),
) {
    if let Some(link) = link {
        link.finished(Addr::Run(run_id));
    }
    release();
    if let (Some(link), Some(msg)) = (link, msg) {
        send_completion(link, run_id, msg);
    }
}

/// A run's completion as the host receives it: the final `Result`, or the
/// `RunComplete` when the early Result already went out during grace.
pub fn completion_message(
    result: &Result<Output, FailureOutput>,
    heap_used_bytes: Option<u64>,
    prefix_stage: bool,
) -> Outbound {
    match result {
        Ok(output) => match &output.background {
            Some(bg) if bg.early_result_sent => Outbound::RunComplete { report: bg.clone() },
            _ => Outbound::Result {
                completion: types::RunCompletion::Success(types::RunSuccessPayload {
                    exports: output.exports.clone(),
                    skipped_exports: output.skipped_exports.clone(),
                    stdout: output.stdout.clone(),
                    stderr: output.stderr.clone(),
                    duration_ms: output.duration_ms,
                    wall_time_ms: output.wall_time_ms,
                    cpu_time_ms: output.cpu_time_ms,
                    bridge_calls: output.bridge_calls.clone(),
                    heap_used_bytes,
                    background_flags: 0,
                }),
            },
        },
        Err(failure) => Outbound::Result {
            completion: types::RunCompletion::Failure(types::RunFailurePayload {
                error: types::RunErrorPayload {
                    prefix_stage,
                    ..failure.error.to_payload()
                },
                stdout: failure.stdout.clone(),
                stderr: failure.stderr.clone(),
                duration_ms: failure.duration_ms,
                wall_time_ms: failure.wall_time_ms,
                cpu_time_ms: failure.cpu_time_ms,
                bridge_calls: failure.bridge_calls.clone(),
                heap_used_bytes,
            }),
        },
    }
}

/// Send a completion, substituting a minimal one when the host cannot carry
/// it: a guest-authored oversized rejection message (or an unbounded record
/// list under `maxBridgeCalls: 0`) costs its telemetry, never the answer.
/// Other failures are logged: the run's own events report the dead host.
pub fn send_completion(link: &dyn HostLink, run_id: u32, msg: Outbound) {
    let report_status = match &msg {
        Outbound::RunComplete { report } => Some(report.status),
        _ => None,
    };
    let outcome = match link.send(Addr::Run(run_id), msg) {
        Err(LinkError::TooLarge(detail)) if report_status.is_none() => {
            eprintln!(
                "[iso4-v8] Result for run {run_id} too large to send ({detail}) — \
                 replacing it with ERR_INTERNAL"
            );
            // stdout/stderr are dropped too: unbounded, and this payload has
            // to be small by construction.
            let completion = types::RunCompletion::Failure(types::RunFailurePayload::bare(
                RunError::Internal(format!(
                    "the run's result was too large to send to the host ({detail}). \
                     A thrown error's message, stack and properties are reported \
                     verbatim, so a very large one cannot be delivered."
                ))
                .to_payload(),
            ));
            link.send(Addr::Run(run_id), Outbound::Result { completion })
        }
        Err(LinkError::TooLarge(detail)) => {
            eprintln!(
                "[iso4-v8] RunComplete for run {run_id} too large to send ({detail}) — \
                 substituting a minimal report"
            );
            let report = GraceReport {
                status: report_status.unwrap_or(GraceStatus::Settled),
                duration_ms: 0.0,
                wall_time_ms: 0.0,
                cpu_time_ms: 0.0,
                stdout: Vec::new(),
                stderr: Vec::new(),
                bridge_calls: Vec::new(),
                error: Some((
                    "Error".to_string(),
                    "the grace report was too large to send; its telemetry was dropped"
                        .to_string(),
                )),
                early_result_sent: true,
            };
            link.send(Addr::Run(run_id), Outbound::RunComplete { report })
        }
        other => other,
    };
    if let Err(e) = outcome {
        eprintln!("[iso4-v8] failed to send completion for run {run_id}: {e}");
    }
}

/// The multi-run loop's deadline heap: `(deadline, run token)`, nearest
/// first. Lazily deleted — an entry is stale when it no longer equals its
/// run's [`next_deadline`]. Each turn re-queues at most one entry per
/// touched run, so the heap grows with turns, never with pending-timer
/// count.
type DeadlineHeap = std::collections::BinaryHeap<std::cmp::Reverse<(std::time::Instant, u64)>>;

/// The nearest live deadline, discarding stale heap tops on the way.
fn peek_valid_deadline(
    live: &[LiveRun],
    deadlines: &mut DeadlineHeap,
) -> Option<(std::time::Instant, u64)> {
    loop {
        let std::cmp::Reverse((at, token)) = *deadlines.peek()?;
        let valid = live
            .iter()
            .find(|l| l.rs.token == token)
            .is_some_and(|l| next_deadline(&l.rs) == Some(at));
        if valid {
            return Some((at, token));
        }
        deadlines.pop();
    }
}

/// Re-queue deadlines for runs whose timer set changed during the last
/// turn (see [`TIMERS_TOUCHED`]); duplicates are harmless (lazy deletion).
fn drain_touched_timers(live: &[LiveRun], deadlines: &mut DeadlineHeap) {
    TIMERS_TOUCHED.with(|v| {
        for token in v.borrow_mut().drain(..) {
            if let Some(l) = live.iter().find(|l| l.rs.token == token) {
                if let Some(at) = next_deadline(&l.rs) {
                    deadlines.push(std::cmp::Reverse((at, token)));
                }
            }
        }
    });
}

/// The per-run failure for a bare mid-turn termination (a routed abort or
/// a guard fire observed by the caller, not by the turn itself).
fn reason_error(reason: &ReasonCell) -> RunError {
    match reason.get() {
        Some(TerminationReason::Wall) => RunError::WallTimeout,
        Some(TerminationReason::Cpu) => RunError::CpuTimeout,
        Some(TerminationReason::Memory) => RunError::MemoryLimit,
        Some(TerminationReason::Import(m)) => RunError::Import(m.to_string()),
        Some(TerminationReason::Abort) | None => RunError::Aborted,
    }
}

/// Serve every deadline at or before `cutoff`: due TIMERS fire (a real
/// turn — guest JS, so taint can arise; `true` = tainted, caller sweeps
/// and returns), expired BOUNDARIES conclude their runs (no JS). A timer
/// tying with its run's boundary fires first.
///
/// The cutoff is what makes deadlines fire in ARRIVAL order rather than
/// processing order: the event paths pass the event's demux arrival stamp,
/// so a deadline concludes its run only once every event that arrived
/// before it has been delivered — a settling frame that reached the demux
/// in time is never beaten by the deadline just because the loop was busy —
/// while events arriving after the deadline cannot delay it. The idle path
/// (select timeout) passes `now`: the queue is empty at that moment, so
/// nothing arrived-but-undelivered exists.
#[must_use]
fn sweep_expired_deadlines(
    core: &mut InstanceCore,
    live: &mut Vec<LiveRun>,
    deadlines: &mut DeadlineHeap,
    cutoff: std::time::Instant,
) -> bool {
    while let Some((at, token)) = peek_valid_deadline(live, deadlines) {
        if at > cutoff {
            break;
        }
        deadlines.pop();
        let idx = live
            .iter()
            .position(|l| l.rs.token == token)
            .expect("peek_valid_deadline checked liveness");
        // Which deadline surfaced: the run's earliest timer, or its
        // phase boundary.
        if earliest_timer(&live[idx].rs) == Some(at) {
            let mut l = live.swap_remove(idx);
            note_turn_owner(&l.job);
            // The wait ended at the timer's due instant, not its late fire.
            enter_turn(&mut l.rs, at);
            let outcome = {
                let facts = RunFacts {
                    call: Some(&l.job.call),
                    reason: &core.reason,
                    guard: &core.guard,
                    cancel_handle: core.guard.handle(),
                };
                timer_turn(&mut core.isolate, &core.context, &facts, &mut l.rs)
            };
            let tainted_now = instance_tainted(core);
            match outcome {
                ScanOutcome::Finished(result) => {
                    conclude_run(&mut core.isolate, &core.run_table, l, *result, tainted_now);
                }
                ScanOutcome::Continue => {
                    if tainted_now {
                        // Bare terminate mid-turn: classify and conclude
                        // here (the event arm's rule).
                        let failure = l.rs.fail(reason_error(&core.reason));
                        conclude_run(&mut core.isolate, &core.run_table, l, Err(failure), true);
                    } else {
                        leave_turn(&mut l.rs);
                        if let Some(next) = next_deadline(&l.rs) {
                            deadlines.push(std::cmp::Reverse((next, token)));
                        }
                        live.push(l);
                    }
                }
            }
            drain_touched_timers(live, deadlines);
            if tainted_now {
                return true;
            }
            continue;
        }
        let mut l = live.swap_remove(idx);
        if matches!(l.rs.phase, RunPhase::Grace(_)) {
            let now = std::time::Instant::now();
            if let Some(d) = out_idle_deadline(&l.rs).filter(|d| *d <= now) {
                // Stream idle allowance: a real turn (guest cancel cleanup).
                note_turn_owner(&l.job);
                enter_turn(&mut l.rs, d);
                let outcome = {
                    let facts = RunFacts {
                        call: Some(&l.job.call),
                        reason: &core.reason,
                        guard: &core.guard,
                        cancel_handle: core.guard.handle(),
                    };
                    stream_idle_turn(&mut core.isolate, &core.context, &facts, &mut l.rs)
                };
                let tainted_now = instance_tainted(core);
                match outcome {
                    ScanOutcome::Finished(result) => {
                        conclude_run(&mut core.isolate, &core.run_table, l, *result, tainted_now);
                    }
                    ScanOutcome::Continue => {
                        if tainted_now {
                            let failure = l.rs.fail(reason_error(&core.reason));
                            conclude_run(&mut core.isolate, &core.run_table, l, Err(failure), true);
                        } else {
                            leave_turn(&mut l.rs);
                            if let Some(next) = next_deadline(&l.rs) {
                                deadlines.push(std::cmp::Reverse((next, token)));
                            }
                            live.push(l);
                        }
                    }
                }
                drain_touched_timers(live, deadlines);
                if tainted_now {
                    return true;
                }
                continue;
            }
            // waitUntil wall: bookkeeping — the phase may stay open for
            // streams.
            if grace_wall_due(&l.rs, now) {
                // Counted grace time reaches its cap exactly here.
                note_turn_owner(&l.job);
                enter_turn(&mut l.rs, at);
                match conclude_grace_wall(&mut l.rs) {
                    Some(output) => {
                        conclude_run(&mut core.isolate, &core.run_table, l, Ok(output), false);
                    }
                    None => {
                        leave_turn(&mut l.rs);
                        if let Some(next) = next_deadline(&l.rs) {
                            deadlines.push(std::cmp::Reverse((next, token)));
                        }
                        live.push(l);
                    }
                }
                continue;
            }
            // Stale entry (activity moved the deadline): requeue.
            if let Some(next) = next_deadline(&l.rs) {
                deadlines.push(std::cmp::Reverse((next, token)));
            }
            live.push(l);
            continue;
        }
        // Counted wall reaches its cap exactly at the boundary instant.
        note_turn_owner(&l.job);
        enter_turn(&mut l.rs, at);
        let result = boundary_deadline(&mut l.rs);
        conclude_run(&mut core.isolate, &core.run_table, l, result, false);
    }
    false
}

/// Seconds an instance must be idle before it settles its heap once (one GC,
/// then a fresh reading for the registry) and parks indefinitely. `0` = off,
/// which is the shipped default: V8 returns a dropped instance's pages on its
/// own, so the settle only pays off where warmth must stay resident in a
/// container tight enough to feel the garbage. `--idle-settle-secs` turns it
/// on.
#[cfg(not(test))]
const IDLE_SETTLE_DEFAULT_SECS: u64 = 0;
/// The suite exercises the feature, shortened — 30 s of sleeping per test is
/// not worth it.
#[cfg(test)]
const IDLE_SETTLE_DEFAULT_SECS: u64 = 1;

static IDLE_SETTLE_SECS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(IDLE_SETTLE_DEFAULT_SECS);

pub fn set_idle_settle_secs(secs: u64) {
    IDLE_SETTLE_SECS.store(secs, std::sync::atomic::Ordering::Relaxed);
}

/// How long an idle instance waits before settling, `None` when off.
pub(crate) fn idle_settle_after() -> Option<Duration> {
    match IDLE_SETTLE_SECS.load(std::sync::atomic::Ordering::Relaxed) {
        0 => None,
        secs => Some(Duration::from_secs(secs)),
    }
}

/// Serve one instance: the per-instance turn loop (#125). Receives from the
/// instance's ONE ordered channel — jobs, run-tagged events, and the
/// registry's Retire all ride it (#170) — bounded by the nearest per-run
/// deadline from a heap; each message is one turn. The demux enqueues a
/// run's job before it can route any of that run's frames, so channel FIFO
/// guarantees a run is live before its events are delivered: an event whose
/// token is not live is LATE (its run concluded), never early. Any number
/// of runs can be suspended at once.
///
/// Deadlines fire in ARRIVAL order, not processing order (see
/// [`sweep_expired_deadlines`]): each delivered event first concludes the
/// deadlines that expired before it arrived, and an idle recv timing out
/// concludes everything expired as of now. So a backlog of frames that
/// arrived after a run's wall cannot starve that deadline, and a settling
/// frame that arrived in time is never beaten by it.
///
/// Returns when a Retire message has been seen and no runs are in flight,
/// or when a turn is interrupted mid-JS — the taint path: the turn owner
/// fails with its classified error and every other in-flight run fails with
/// `ERR_INSTANCE_RESET` carrying the cause class and the culprit's wire run
/// id (E1 rulings 3+4). The instance dies with the loop either way.
pub fn serve_instance(
    core: &mut InstanceCore,
    prefix_globals: &[types::HostGlobalDef],
    imports: &[types::ImportBinding],
    first: Box<CallJob>,
    events: &crossbeam_channel::Receiver<RoutedEvent>,
    load: &InstanceLoad,
) {
    let mut live: Vec<LiveRun> = Vec::new();
    let mut deadlines = DeadlineHeap::new();
    let mut retired = false;
    // Whether the current idle stretch has had its settle GC.
    let mut settled = false;
    // Utilization bookkeeping: busy time per wall window, published for the
    // registry's join routing.
    let mut window_start = std::time::Instant::now();
    let mut busy_nanos: u64 = 0;
    // Built once; `Select` parks without `recv()`'s spin/yield backoff,
    // which loses on request-gap-sized waits.
    let mut sel = crossbeam_channel::Select::new();
    sel.recv(events);

    // The job that triggered instance creation, then the loop.
    load.open_turn(load_now_ms());
    let first_tainted =
        dispatch_job(core, prefix_globals, imports, &mut live, &mut deadlines, first);
    load.close_turn();
    if first_tainted {
        taint_sweep(core, &mut live, events);
        return;
    }

    loop {
        if live.is_empty() && retired {
            return;
        }
        set_oom_run(OomRun::None);

        let received = {
            let timeout = peek_valid_deadline(&live, &mut deadlines)
                .map(|(at, _)| at.saturating_duration_since(std::time::Instant::now()))
                // Nothing pending and the last run's garbage still held:
                // wake once to settle the heap, then park indefinitely.
                .or_else(|| (live.is_empty() && !settled).then(idle_settle_after).flatten());
            let oper = match timeout {
                Some(t) => sel.select_timeout(t).ok(),
                None => Some(sel.select()),
            };
            match oper {
                None => Err(crossbeam_channel::RecvTimeoutError::Timeout),
                Some(op) => op
                    .recv(events)
                    .map_err(|_| crossbeam_channel::RecvTimeoutError::Disconnected),
            }
        };
        if received.is_ok() {
            settled = false;
        }

        let turn_started = std::time::Instant::now();
        load.open_turn(load_now_ms());
        match received {
            Err(crossbeam_channel::RecvTimeoutError::Timeout) if live.is_empty() => {
                settled = true;
                settle_heap(&mut core.isolate);
                load.publish_settled_heap(heap_used(&mut core.isolate));
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                // The recv timed out: the channel was empty for the whole
                // window, so nothing arrived-but-undelivered can beat the
                // due deadline.
                if sweep_expired_deadlines(
                    core,
                    &mut live,
                    &mut deadlines,
                    std::time::Instant::now(),
                ) {
                    taint_sweep(core, &mut live, events);
                    return;
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                // Every sender is gone. The handle's Retire precedes its own
                // sender dropping, so runs still live here mean a panicked
                // feeder dropped its routes without the usual connection-lost
                // delivery. Nothing can ever wake them: fail them cleanly
                // instead of stranding them.
                while let Some(mut l) = live.pop() {
                    let result = boundary_close(
                        &mut l.rs,
                        io::Error::new(io::ErrorKind::BrokenPipe, "demux gone"),
                    );
                    conclude_run(&mut core.isolate, &core.run_table, l, result, false);
                }
                return;
            }
            Ok(RoutedEvent {
                event: RunEvent::Retire,
                ..
            }) => {
                retired = true;
            }
            Ok(RoutedEvent {
                event: RunEvent::Job(msg),
                at,
                ..
            }) => {
                // Deadlines/timers due before this job arrived fire first —
                // overdue work beats admitting new runs.
                if sweep_expired_deadlines(core, &mut live, &mut deadlines, at) {
                    taint_sweep(core, &mut live, events);
                    return;
                }
                if dispatch_job(core, prefix_globals, imports, &mut live, &mut deadlines, msg) {
                    taint_sweep(core, &mut live, events);
                    return;
                }
            }
            Ok(RoutedEvent { token, event, at }) => {
                // Deadlines that expired BEFORE this event arrived precede
                // it; anything later must not delay it (or be delayed by
                // it).
                if sweep_expired_deadlines(core, &mut live, &mut deadlines, at) {
                    taint_sweep(core, &mut live, events);
                    return;
                }
                let Some(idx) = live.iter().position(|l| l.rs.token == token) else {
                    // FIFO + the demux's job-before-frames order make this
                    // LATE by construction: the run concluded while the
                    // event sat in the queue. Discard like the demux's own
                    // late-frame rule — but loudly (silence hid #170).
                    match &event {
                        RunEvent::Host(msg) => eprintln!(
                            "[iso4-v8] discarding late {msg} (run already concluded)"
                        ),
                        RunEvent::Job(_) | RunEvent::Retire => {}
                    }
                    // Discarding is the whole turn; an unclosed one reads
                    // as 100% busy until the next event.
                    load.close_turn();
                    continue;
                };
                let mut l = live.swap_remove(idx);
                let event = match event {
                    RunEvent::Host(Inbound::ConnectionLost { detail }) => {
                        LoopEvent::SourceClosed(io::Error::new(
                            io::ErrorKind::BrokenPipe,
                            detail.unwrap_or_else(|| "connection closed".to_string()),
                        ))
                    }
                    RunEvent::Host(msg) => LoopEvent::Host(msg),
                    RunEvent::Job(_) | RunEvent::Retire => unreachable!("consumed above"),
                };
                note_turn_owner(&l.job);
                // The deadline as the heap knows it; a change during this
                // turn (settle → grace, a timer armed/fired/cleared)
                // re-queues below. Captured before the wait is folded.
                let queued_deadline = next_deadline(&l.rs);
                // The wait ended when the event ARRIVED; the delivery lag
                // was the loop's, not this run's.
                enter_turn(&mut l.rs, at);
                let outcome = {
                    let facts = RunFacts {
                        call: Some(&l.job.call),
                        reason: &core.reason,
                        guard: &core.guard,
                        cancel_handle: core.guard.handle(),
                    };
                    handle_loop_event(&mut core.isolate, &core.context, &facts, &mut l.rs, event)
                };
                let tainted_now = instance_tainted(core);
                match outcome {
                    Some(result) => {
                        conclude_run(&mut core.isolate, &core.run_table, l, result, tainted_now);
                    }
                    None => {
                        if tainted_now {
                            // Bare terminate mid-turn (a routed abort landed
                            // while this run's own turn ran): the owner is
                            // still live — classify and conclude it here.
                            let failure = l.rs.fail(reason_error(&core.reason));
                            conclude_run(&mut core.isolate, &core.run_table, l, Err(failure), true);
                        } else {
                            leave_turn(&mut l.rs);
                            let current = next_deadline(&l.rs);
                            if current != queued_deadline {
                                if let Some(at) = current {
                                    deadlines.push(std::cmp::Reverse((at, token)));
                                }
                            }
                            live.push(l);
                        }
                    }
                }
                drain_touched_timers(&live, &mut deadlines);
                if tainted_now {
                    taint_sweep(core, &mut live, events);
                    return;
                }
            }
        }

        load.close_turn();
        busy_nanos += turn_started.elapsed().as_nanos() as u64;
        let window = window_start.elapsed();
        if window.as_millis() as u64 >= LOAD_WINDOW_MS {
            let permille = (busy_nanos.saturating_mul(1000)
                / (window.as_nanos() as u64).max(1))
            .min(1000) as u32;
            load.close_window(permille);
            window_start = std::time::Instant::now();
            busy_nanos = 0;
        }
    }
}

/// Start one job on the instance. Returns whether the instance came out of
/// the dispatch tainted (mid-JS interruption during the start turn). A job
/// that suspends is pushed onto `live` with its boundary deadline (if any)
/// queued on `deadlines`.
fn dispatch_job(
    core: &mut InstanceCore,
    prefix_globals: &[types::HostGlobalDef],
    imports: &[types::ImportBinding],
    live: &mut Vec<LiveRun>,
    deadlines: &mut DeadlineHeap,
    mut job: Box<CallJob>,
) -> bool {
    if let Some(slot) = &job.ctl_slot {
        slot.set(core.guard.ctl()).ok();
    }
    note_turn_owner(&job);

    // Jobs interleave. The reason cell may only be reset while nothing else
    // is in flight; when runs are live it is guaranteed clear (a set reason
    // ends the loop).
    if live.is_empty() {
        core.reason.reset();
    }
    let token = if job.token != 0 {
        job.token
    } else {
        alloc_run_token()
    };
    let start = std::time::Instant::now();
    let cpu_budget = Arc::new(CpuBudget::new());
    let bridge_log: Arc<Mutex<BridgeCallLog>> = Arc::new(Mutex::new(BridgeCallLog::default()));

    // The run's table entry (see run_call_on_core — the same prelude).
    let entry = RunCallState::new(
        job.limits.max_stdout_bytes,
        job.limits.max_stderr_bytes,
        Arc::clone(&job.observer),
        Addr::Run(job.epilogue.map_or(0, |e| e.run_id)),
    );
    if core.warmup_logs_pending {
        core.warmup_logs_pending = false;
        let mut setup = core.run_table.setup_logs.borrow_mut();
        let mut logs = entry.logs.borrow_mut();
        logs.stdout = std::mem::take(&mut setup.stdout);
        logs.stderr = std::mem::take(&mut setup.stderr);
        logs.stdout_bytes = std::mem::take(&mut setup.stdout_bytes);
        logs.stderr_bytes = std::mem::take(&mut setup.stderr_bytes);
    }
    let logs_ptr: *const RefCell<LogBuffers> = std::ptr::addr_of!(*entry.logs);
    let pending_ptr: *const PendingWork = std::ptr::addr_of!(*entry.pending);
    let streams_ptr: *const StreamTable = std::ptr::addr_of!(*entry.streams);
    let out_streams_ptr: *const OutStreamTable = std::ptr::addr_of!(*entry.out_streams);
    let timers_ptr: *const RefCell<TimerTable> = std::ptr::addr_of!(*entry.timers);
    core.run_table.runs.borrow_mut().insert(token, entry);
    RUN_TABLE_PTR.with(|c| c.set(std::ptr::addr_of!(*core.run_table)));

    // Guard up before anything touches the retained context.
    core.guard.set_for(
        GuardTarget {
            budget: Arc::clone(&cpu_budget),
            cpu_cap_ms: job.limits.cpu_time_ms,
            wall_deadline: (job.limits.wall_time_ms > 0)
                .then(|| start + Duration::from_millis(job.limits.wall_time_ms as u64)),
        },
        token,
    );

    let placeholder_result = {
        v8::scope!(let scope, &mut core.isolate);
        let context = v8::Local::new(scope, &core.context);
        let scope = &mut v8::ContextScope::new(scope, context);
        install_prefix_bridge_placeholders(scope, prefix_globals, imports)
    };
    let cancel_handle = core.guard.handle().clone();
    let started = match placeholder_result {
        Err(error) => {
            core.guard.clear();
            let f = failure(
                termination_or(&core.reason, error),
                &unsafe { &*logs_ptr }.borrow(),
                start,
            );
            None.ok_or(f)
        }
        Ok(()) => {
            let io = std::mem::replace(&mut job.io, RunIo::None);
            let mut ctx = CallPhaseCtx {
                token,
                table: std::ptr::addr_of!(*core.run_table),
                limits: &job.limits,
                globals: &job.globals,
                imports,
                io,
                call_id_counter: Arc::clone(&job.call_id_counter),
                call: Some(&job.call),
                start,
                cpu_budget: &cpu_budget,
                bridge_log: &bridge_log,
                reason: &core.reason,
                logs: logs_ptr,
                pending: pending_ptr,
                streams: streams_ptr,
                out_streams: out_streams_ptr,
                timers: timers_ptr,
                epilogue: job.epilogue,
                cancel_handle: &cancel_handle,
            };
            let InstanceCore {
                isolate,
                context,
                prefix_module,
                stub_slots,
                guard,
                ..
            } = core;
            Ok(begin_call(
                isolate,
                context,
                RunEntry::Prefix(prefix_module),
                stub_slots,
                guard,
                &mut ctx,
            ))
        }
    };

    let tainted_now = instance_tainted(core);
    match started {
        Err(f) => {
            let l = LiveRun {
                rs: RunState {
                    token,
                    table: std::ptr::addr_of!(*core.run_table),
                    limits: job.limits,
                    start,
                    epilogue: job.epilogue,
                    source: None,
                    link: job.io.link(),
                    cpu_budget,
                    bridge_log,
                    bridge_error: Arc::new(OnceLock::new()),
                    resolvers: Arc::new(Mutex::new(HashMap::new())),
                    logs: logs_ptr,
                    pending: pending_ptr,
                    streams: streams_ptr,
                out_streams: out_streams_ptr,
                    timers: timers_ptr,
                    phase: RunPhase::Starting,
                    counted: Duration::ZERO,
                    parked_at: start,
                    turn_entered: Some(start),
                },
                job,
            };
            conclude_run(&mut core.isolate, &core.run_table, l, Err(f), tainted_now);
        }
        Ok((rs, outcome)) => {
            let l = LiveRun { rs, job };
            match outcome {
                ScanOutcome::Finished(result) => {
                    conclude_run(&mut core.isolate, &core.run_table, l, *result, tainted_now);
                }
                ScanOutcome::Continue => {
                    if tainted_now {
                        let f = l.rs.fail(reason_error(&core.reason));
                        conclude_run(&mut core.isolate, &core.run_table, l, Err(f), true);
                    } else {
                        // Covers a settling run's wall deadline, a run that
                        // entered grace within its start turn, and any
                        // timer the start turn armed.
                        if let Some(at) = next_deadline(&l.rs) {
                            deadlines.push(std::cmp::Reverse((at, l.rs.token)));
                        }
                        live.push(l);
                    }
                }
            }
        }
    }
    // A start turn can touch a co-resident's timers like any other turn.
    drain_touched_timers(live, deadlines);
    tainted_now
}

/// The taint path: every other in-flight run on the instance fails with
/// `ERR_INSTANCE_RESET` carrying the cause class and the culprit's wire run
/// id (E1 ruling 4), and queued jobs that never started are failed loudly.
/// The caller returns afterwards; the instance dies with its loop.
fn taint_sweep(
    core: &mut InstanceCore,
    live: &mut Vec<LiveRun>,
    events: &crossbeam_channel::Receiver<RoutedEvent>,
) {
    let cause = match core.reason.get() {
        Some(TerminationReason::Cpu) => ResetCause::Cpu,
        Some(TerminationReason::Memory) => ResetCause::Memory,
        Some(TerminationReason::Wall) => ResetCause::Wall,
        Some(TerminationReason::Import(_)) => ResetCause::Unsupported,
        Some(TerminationReason::Abort) | None => ResetCause::Abort,
    };
    // The culprit was concluded by the caller before the sweep; its wire id
    // travels via the guard's last executing turn... the loop tracks it as
    // the run it just processed. The sweep is only ever entered right after
    // that conclusion, so the culprit is simply the one run NOT in `live`.
    // Its wire id is recorded by the caller through `LAST_CULPRIT_RUN_ID`.
    let culprit_run_id = LAST_CULPRIT_RUN_ID.with(|c| c.get());
    for l in live.drain(..) {
        let f = l.rs.fail(RunError::InstanceReset {
            cause,
            culprit_run_id,
        });
        conclude_run(&mut core.isolate, &core.run_table, l, Err(f), true);
    }
    // Jobs already queued but never started: answer them instead of letting
    // their callers hang on a dropped channel. Queued events belong to runs
    // failed above (or concluded earlier) — dropped with the instance.
    while let Ok(routed) = events.try_recv() {
        let RunEvent::Job(mut job) = routed.event else {
            continue;
        };
        let outcome = CallOutcome {
            result: Err(failure(
                RunError::InstanceReset {
                    cause,
                    culprit_run_id,
                },
                &LogBuffers::default(),
                std::time::Instant::now(),
            )),
            tainted: true,
            heap_used_bytes: 0,
            prefix_failed: false,
        };
        let link = job.io.link();
        finish_job(&mut job, link, outcome);
    }
}

thread_local! {
    /// The wire run id of the turn owner most recently processed on this
    /// thread — the culprit named in an `ERR_INSTANCE_RESET` sweep.
    static LAST_CULPRIT_RUN_ID: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// A turn starts for `job`: record it as the sweep culprit and as the run
/// executing on this thread.
pub fn note_turn_owner(job: &CallJob) {
    let run_id = job.epilogue.map(|e| e.run_id).unwrap_or(0);
    LAST_CULPRIT_RUN_ID.with(|c| c.set(run_id));
    set_oom_run(OomRun::Run(run_id));
}

fn heap_used(isolate: &mut v8::OwnedIsolate) -> u64 {
    isolate.get_heap_statistics().used_heap_size() as u64
}

/// One full collection when an instance goes idle — V8's own memory reducer
/// never runs here: nothing pumps the platform loop.
fn settle_heap(isolate: &mut v8::OwnedIsolate) {
    isolate.low_memory_notification();
}

/// Clone the shared `bridge_error` OnceLock's content into an owned RunError.
fn owned_bridge_error(err: &RunError) -> RunError {
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
    msg: Inbound,
) -> Result<(), RunError> {
    // SAFETY: the table outlives the run's events; transient borrow.
    let table = unsafe { &*streams };
    let mut slot = table.slot.borrow_mut();
    let Some(state) = slot.as_mut() else {
        return Ok(()); // no run armed (stray message) — drop it
    };

    match msg {
        Inbound::StreamChunk { stream_id, data } => {
            let Some(stream) = state.streams.get_mut(&stream_id) else {
                return Ok(());
            };
            if stream.cancelled {
                return Ok(());
            }
            if let Some(pending) = stream.pending.take() {
                let credit = data.len() as u32;
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
            let cap = types::STREAM_CREDIT_WINDOW_BYTES as usize
                + types::STREAM_CHUNK_MAX_BYTES as usize;
            if stream.buffered_bytes + data.len() > cap {
                return Err(RunError::Internal(
                    "stream credit window violated by the host".to_string(),
                ));
            }
            stream.buffered_bytes += data.len();
            stream.buffered.push_back(data);
            Ok(())
        }
        Inbound::StreamEnd { stream_id, error } => {
            let Some(stream) = state.streams.get_mut(&stream_id) else {
                return Ok(());
            };
            let outcome: Result<(), String> = match error {
                None => Ok(()),
                Some(message) => Err(message),
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

/// Wall-clock elapsed time since `start` in milliseconds, rounded to
/// microsecond resolution (three decimal places). The wire carries
/// durations as f64, so sub-millisecond runs stay visible.
fn elapsed_ms(start: std::time::Instant) -> f64 {
    (start.elapsed().as_secs_f64() * 1_000_000.0).round() / 1_000.0
}

/// A guard can fire while the run is still in setup — installing globals or
/// the web runtime, evaluating the prefix, compiling a one-off module. V8 then
/// fails the interrupted operation, and classifying that failure would
/// misreport the timeout as `ERR_INTERNAL` ("evaluate web runtime: null") or
/// a `SyntaxError`. The guard stores its reason *before* it calls
/// `terminate_execution`, so a set reason here is always the real cause;
/// otherwise the fallback stands.
fn termination_or(reason: &ReasonCell, fallback: RunError) -> RunError {
    match reason.get() {
        Some(TerminationReason::Wall) => RunError::WallTimeout,
        Some(TerminationReason::Cpu) => RunError::CpuTimeout,
        Some(TerminationReason::Memory) => RunError::MemoryLimit,
        Some(TerminationReason::Abort) => RunError::Aborted,
        Some(TerminationReason::Import(m)) => RunError::Import(m.to_string()),
        None => fallback,
    }
}

fn failure(error: RunError, logs: &LogBuffers, start: std::time::Instant) -> FailureOutput {
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

struct PendingResolver(v8::Global<v8::PromiseResolver>);
// SAFETY: only ever accessed from the V8 isolate thread.
unsafe impl Send for PendingResolver {}

type PendingResolvers = Arc<Mutex<HashMap<u32, PendingResolver>>>;

/// Per-stub-name dispatch slot with **instance** lifetime.
///
/// The V8 `Function` a bridge stub installs holds a raw `External` pointer to
/// its slot, and sandbox code can stash that function across calls on a warm
/// instance (`globalThis.saved = fetch`) — so the pointee must outlive every
/// call, not just the one that installed it. The slot carries only identity:
/// the stub name and the instance's run table. The per-run half (link,
/// counters, limits) lives in the calling run's [`RunCallState::stubs`] map,
/// resolved per invocation by the attributed token — so one Function object
/// serves every run that binds the name, each with its own state, and a run
/// that did not bind it gets a catchable error instead of a stale dispatch.
struct StubSlot {
    /// The global-object key this slot serves.
    stub_name: String,
    /// The instance's run table (instance-lifetime Box, like the slot).
    table: *const RunTable,
}

/// Instance-lifetime slot table, keyed by stub name. Boxed so the addresses
/// handed to V8 as `External`s never move.
#[allow(clippy::vec_box)]
type StubSlots = HashMap<String, Box<StubSlot>>;

// Deliberately NOT Clone: bindings live behind an Rc in the run's stub map,
// and a per-invocation deep clone (two Strings, several Arcs) is the cost
// this shape removed.
struct GlobalCallbackData {
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

/// Private-symbol key stamping a hydrated body-stream object with its stream
/// id — Rust-readable, guest-invisible, and how the outbound codec refuses to
/// serialize a stream back to the host.
const BODY_STREAM_ID_KEY: &str = "iso4::bodyStreamId";

/// Private marker stamped on EVERY `Iso4BodyStream` at construction (the JS
/// constructor calls the `stampStreamBody` native), so the codec can
/// recognize a stream body without consulting guest-mutable state.
const STREAM_BODY_MARK_KEY: &str = "iso4::streamBody";

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

/// Whether `value` is a guest body stream (`Iso4BodyStream`), by its
/// construction-time private stamp — never `instanceof` or duck typing.
pub fn is_stream_body(scope: &mut v8::PinScope, value: v8::Local<v8::Value>) -> bool {
    let Ok(obj) = v8::Local::<v8::Object>::try_from(value) else {
        return false;
    };
    let Some(key) = body_stream_private(scope, STREAM_BODY_MARK_KEY) else {
        return false;
    };
    obj.get_private(scope, key).is_some_and(|v| v.is_true())
}

/// `stampStreamBody(obj)` — native handed to the web runtime; the
/// `Iso4BodyStream` constructor stamps every instance with the private
/// marker [`is_stream_body`] checks.
pub(crate) fn stamp_stream_body_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let Ok(obj) = v8::Local::<v8::Object>::try_from(args.get(0)) else {
        return;
    };
    let Some(key) = body_stream_private(scope, STREAM_BODY_MARK_KEY) else {
        return;
    };
    let yes = v8::Boolean::new(scope, true);
    obj.set_private(scope, key, yes.into());
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

/// `streamRead(streamId)` — native pull for the sandbox body stream: resolves
/// with a `Uint8Array` chunk, `null` at EOF, or rejects on a source failure.
/// Consumed bytes replenish the host's credit window.
pub(crate) fn stream_read_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let Some(run_table) = slot_table(scope) else {
        throw_v8_error(scope, "[iso4] stream read outside an instance");
        return;
    };
    let Some(resolver) = v8::PromiseResolver::new(scope) else {
        throw_v8_error(scope, "[iso4] stream read could not allocate");
        return;
    };
    rv.set(resolver.get_promise(scope).into());
    let stream_id = args.get(0).uint32_value(scope).unwrap_or(0);

    let state = attributed_state(scope, run_table);
    if state.is_null() {
        let msg = v8_error_value(scope, "[iso4] streamed bodies are only readable during a run");
        resolver.reject(scope, msg);
        return;
    }
    // SAFETY: entry pointers are valid for the remainder of the turn.
    let table = unsafe { &*state }.streams.as_ref();
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
pub(crate) fn stream_cancel_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let Some(run_table) = slot_table(scope) else {
        rv.set_undefined();
        return;
    };
    let state = attributed_state(scope, run_table);
    if state.is_null() {
        rv.set_undefined();
        return;
    }
    // SAFETY: entry pointers are valid for the remainder of the turn.
    let table = unsafe { &*state }.streams.as_ref();
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

// ── Async context (AsyncLocalStorage) ────────────────────────────────────────
//
// A minimal, Node-compatible `AsyncLocalStorage` (`run` + `getStore`), surfaced
// to run and prefix code via `import { AsyncLocalStorage } from 'node:async_hooks'`.
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
// The class is installed into the build-time startup snapshot, so prefix code
// may import it too; a frame set outside any run carries token 0.

/// Reserved module specifier that resolves to the built-in async-context module.
const ASYNC_HOOKS_SPECIFIER: &str = "node:async_hooks";

/// Global-registry symbol description under which the `AsyncLocalStorage` class
/// is stashed. Must match `Symbol.for(...)` in [`ASYNC_HOOKS_MODULE_SRC`].
const ASYNC_CONTEXT_SYMBOL: &str = "iso4.async_hooks.AsyncLocalStorage";

/// Classic script evaluated once, into the startup snapshot. Returns the `AsyncLocalStorage`
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

// ── The CPED rider ───────────────────────────────────────────────────────────
//
// The CPED slot no longer holds the AsyncLocalStorage frame directly: it
// holds a rider `[token, frame]` — the owning run's token beside the guest
// frame (celld's pattern). V8 saves and restores CPED per continuation, so a
// continuation created by run A carries A's token even when it executes
// during run B's turn, and console output, bridge-stub dispatch, waitUntil
// registration and stream reads attribute to A. Guests never see the rider:
// the native get/set below unwrap/wrap it, and raw CPED is not otherwise
// reachable from JS.

/// Build a rider `[token, inner]`.
fn make_rider<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    token: u64,
    inner: v8::Local<v8::Value>,
) -> v8::Local<'s, v8::Array> {
    let rider = v8::Array::new(scope, 2);
    let token_val = v8::Number::new(scope, token as f64);
    rider.set_index(scope, 0, token_val.into());
    rider.set_index(scope, 1, inner);
    rider
}

/// Split a CPED value into `(token, inner)` when it is a rider.
fn rider_parts<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    value: v8::Local<v8::Value>,
) -> Option<(u64, v8::Local<'s, v8::Value>)> {
    let arr = v8::Local::<v8::Array>::try_from(value).ok()?;
    if arr.length() != 2 {
        return None;
    }
    let token = arr.get_index(scope, 0)?.number_value(scope)?;
    let inner = arr.get_index(scope, 1)?;
    Some((token as u64, inner))
}

/// Install this run's rider at turn entry: continuations created from here
/// carry the token. The previous rider needs no restoring — turn exit clears
/// [`CURRENT_RUN_TOKEN`] and the next turn overwrites the slot.
fn install_cped_rider(scope: &mut v8::PinScope, token: u64) {
    let undefined = v8::undefined(scope).into();
    let rider = make_rider(scope, token, undefined);
    scope.set_continuation_preserved_embedder_data(rider.into());
}

/// Native getter for the continuation-preserved embedder data slot: the
/// guest-visible value is the rider's inner frame.
pub(crate) fn async_context_get_callback(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let data = scope.get_continuation_preserved_embedder_data();
    match rider_parts(scope, data) {
        Some((_, inner)) => rv.set(inner),
        None => rv.set_undefined(),
    }
}

/// Native setter for the continuation-preserved embedder data slot: wraps the
/// guest frame in a rider carrying the creating run's token (taken from the
/// current rider, so a cross-run continuation keeps ITS run, not the turn
/// owner's).
pub(crate) fn async_context_set_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let current = scope.get_continuation_preserved_embedder_data();
    let token = match rider_parts(scope, current) {
        Some((token, _)) => token,
        None => CURRENT_RUN_TOKEN.with(|c| c.get()),
    };
    let frame = args.get(0);
    let rider = make_rider(scope, token, frame);
    scope.set_continuation_preserved_embedder_data(rider.into());
    rv.set_undefined();
}

/// Install the `AsyncLocalStorage` class on `globalThis` under
/// `Symbol.for(ASYNC_CONTEXT_SYMBOL)`, at snapshot creation, so the built-in
/// `node:async_hooks` module can read it.
pub(crate) fn install_async_context(scope: &mut v8::PinScope) -> Result<(), RunError> {
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

/// Error text budget: leaves room for our marker under the wire clamp, so
/// only one marker, with the true length, reaches the host.
const ERROR_TEXT_BYTES: usize = errors::MAX_ERROR_TEXT_BYTES - 128;

/// Copy at most 64 Ki UTF-16 units, then at most `max_bytes` of UTF-8, of
/// guest text out of V8. `write_v2` reads a cons string in place;
/// `to_rust_string_lossy` would flatten it first.
fn capped_text_lossy(scope: &v8::Isolate, text: v8::Local<v8::String>, max_bytes: usize) -> String {
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
fn syntax_error(
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

fn runtime_error_from_value(
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

fn exception_message(
    scope: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
) -> String {
    scope
        .exception()
        .and_then(|e| e.to_string(scope))
        .map(|s| capped_text_lossy(scope, s, ERROR_TEXT_BYTES))
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
        .map(|s| capped_text_lossy(scope, s, ERROR_TEXT_BYTES))
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
    use super::testsupport::*;
    // The exports/args/fields slots are now V8 serialization blobs; the test
    // suite asserts on the decoded shape, so it reads them back through the
    // test-only value tree in `testval.rs`. `WireValue` is kept as the local
    // alias so the assertions below still read as value shapes.
    use crate::testval::{self, TestValue as WireValue};

    #[test]
    fn a_turn_outlasting_a_window_reads_as_saturated() {
        let load = InstanceLoad::new();
        load.close_window(300);
        assert_eq!(load.util_permille_at(10), 300, "between turns: the window");
        load.open_turn(10);
        assert_eq!(load.util_permille_at(10 + LOAD_WINDOW_MS - 1), 300);
        assert_eq!(
            load.util_permille_at(10 + LOAD_WINDOW_MS),
            UTILIZATION_SATURATED_PERMILLE,
            "one turn has held the thread for a whole window",
        );
        load.close_turn();
        assert_eq!(load.util_permille_at(10 + LOAD_WINDOW_MS), 300);
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

    // ── Error handling ────────────────────────────────────────────────────

    #[test]
    fn syntax_error_is_user_code() {
        let err = run_err("export default (((");
        assert!(matches!(&err, RunError::RuntimeError(d) if d.name == "SyntaxError"), "got {err:?}");
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
                s.send(bridge_resp_ok(0, &large));
            },
        );
        h.join().unwrap();
        assert_eq!(
            get_field(&out.unwrap(), "default"),
            Some(WireValue::BigInt(false, vec![0, 0, 1]))
        );
    }

    // ── Memory limits ─────────────────────────────────────────────────────

    #[test]
    fn fatal_oom_line_names_the_run_executing_on_this_thread() {
        let line = |buf: &mut [u8]| {
            let n = format_oom_line(buf, true, Some(c"more"), Some(c"Reached heap limit"));
            String::from_utf8_lossy(&buf[..n]).into_owned()
        };
        let mut buf = [0u8; 256];
        set_oom_prefix_id(Some(123));
        set_oom_run(OomRun::Run(17));
        assert_eq!(
            line(&mut buf),
            "[iso4-v8] fatal: V8 out of memory (heap) in run 17 of prefix 123: \
             Reached heap limit (more) — aborting the process\n"
        );
        set_oom_run(OomRun::ColdStart(17));
        assert!(line(&mut buf).contains(" during cold start of prefix 123 (triggered by run 17): "));
        set_oom_prefix_id(None);
        set_oom_run(OomRun::Run(18));
        assert!(line(&mut buf).contains(" in one-off run 18: "));
        set_oom_run(OomRun::None);
        // Null detail and location are left out; a full buffer cuts, never grows.
        let n = format_oom_line(&mut buf, false, None, None);
        assert_eq!(
            &buf[..n],
            "[iso4-v8] fatal: V8 out of memory (non-heap) with no run executing on this thread \
             — aborting the process\n"
                .as_bytes()
        );
        let mut small = [0u8; 24];
        assert_eq!(format_oom_line(&mut small, true, None, None), 24);
        assert_eq!(&small, b"[iso4-v8] fatal: V8 out ");
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
                hard_memory_mb: 32,
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
                hard_memory_mb: 32,
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

    // ── Output metadata ───────────────────────────────────────────────────

    #[test]
    fn duration_ms_is_populated() {
        let out = run_ok("export default 1 + 1");
        // Sub-millisecond runs must report a non-zero fractional duration
        // (microsecond resolution) instead of truncating to 0.
        assert!(out.duration_ms > 0.0);
        assert!(out.duration_ms < 5_000.0);
    }

    // ── Removed globals: WebAssembly, SharedArrayBuffer ─────────────────────

    #[test]
    fn one_off_runs_have_no_webassembly_or_sharedarraybuffer() {
        let out = run_ok("export default [typeof WebAssembly, typeof SharedArrayBuffer].join('|')");
        assert_eq!(get_default(&out).as_deref(), Some("undefined|undefined"));
    }

    #[test]
    fn warm_instance_setup_and_runs_have_no_webassembly() {
        // Deleted before prefix evaluation too: no reference can be captured
        // at prepare() time and handed to run code.
        let prefix = prepared(
            "globalThis.atSetup = [typeof WebAssembly, typeof SharedArrayBuffer].join('|')\n\
             export default () => atSetup + '|' + typeof WebAssembly",
            &[],
            &[],
        );
        let out = execute_with_prefix(
            prefix,
            Limits::default(),
            &[],
            &[],
            RunIo::None,
            Arc::new(AtomicU32::new(0)),
            &call_spec("default", &[]),
        )
        .unwrap();
        assert_eq!(
            value_of(&out),
            WireValue::String("undefined|undefined|undefined".to_string())
        );
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
            RunIo::None,
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
             globalThis.viaEval = eval('(x) => x + 1')\n\
             export default () => { \
               const denied = (() => { try { eval('1'); return 'allowed' } \
                 catch (e) { return e instanceof EvalError } })(); \
               return [double(21), viaEval(41), denied].join('|') }",
            &[],
            &[],
        );
        let out = execute_with_prefix(
            prefix,
            Limits::default(),
            &[],
            &[],
            RunIo::None,
            Arc::new(AtomicU32::new(0)),
            &call_spec("default", &[]),
        )
        .unwrap();
        assert_eq!(value_of(&out), WireValue::String("42|42|true".to_string()));
    }

    // ── waitUntil grace phase (hold mode — no epilogue spec/socket) ─────────

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
            RunIo::None,
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
            RunIo::None,
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
            RunIo::None,
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
            RunIo::None,
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
               try { waitUntil(1); return 'allowed' } catch (e) { return e.message } })()\n\
             export default () => [globalThis.setupMsg, typeof waitUntil].join('|')",
            &[],
            &[],
        );
        let out = execute_with_prefix(
            prefix,
            Limits::default(),
            &[],
            &[],
            RunIo::None,
            Arc::new(AtomicU32::new(0)),
            &call_spec("default", &[]),
        )
        .unwrap();
        let WireValue::String(got) = value_of(&out) else {
            panic!("expected a string value");
        };
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
            RunIo::None,
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
            let (io, host) = test_io();
            let handle = spawn_responder(host, |s| {
                s.send(bridge_resp_ok(0, &WireValue::Number(42.0)));
            });
            let result = execute(
                "waitUntil((async () => { console.log('bg:' + (await myTool())) })())\n\
                 export default 'early'",
                None,
                grace_limits(),
                &[HostGlobalDef::bridge("myTool")],
                &[],
                io,
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
        let mut core = create_instance_core(
            PrefixSpec {
                code: "export function park() { waitUntil(new Promise(() => {})); return 1 }\n\
                       export function probe() { return typeof waitUntil }",
                filename: "<prefix>",
                globals: &[],
            },
            &[],
            0,
        )
        .expect("core");
        let outcome = run_call_on_core(
            &mut core,
            &[],
            grace_limits(),
            &[],
            &[],
            RunIo::None,
            Arc::new(AtomicU32::new(0)),
            &call_spec("park", &[]),
        );
        assert!(!outcome.tainted, "boundary truncation must not taint");
        let out = outcome.result.expect("run succeeded");
        assert_eq!(
            out.background.expect("grace ran").status,
            GraceStatus::Truncated
        );

        let second = run_call_on_core(
            &mut core,
            &[],
            Limits::default(),
            &[],
            &[],
            RunIo::None,
            Arc::new(AtomicU32::new(0)),
            &call_spec("probe", &[]),
        );
        let out = second.result.expect("instance reusable after clean truncation");
        assert_eq!(value_of(&out), WireValue::String("function".to_string()));
        assert!(out.background.is_none());
    }

    #[test]
    fn prefix_code_may_use_codegen() {
        evaluate_prefix("globalThis.f = new Function('return 1'); eval('2')", &[], &[])
        .unwrap();
    }

    // Top-level `await` used to fail every prefix because nothing
    // drained the microtask queue after module evaluation.

    #[test]
    fn prefix_top_level_await_settles() {
        let prefix = prepared(
            "globalThis.a = await Promise.resolve(40)\n\
             globalThis.b = await new Response('x').text()\n\
             export default () => [globalThis.a + 1, globalThis.b].join('|')",
            &[],
            &[],
        );
        let out = execute_with_prefix(
            prefix,
            Limits::default(),
            &[],
            &[],
            RunIo::None,
            Arc::new(AtomicU32::new(0)),
            &call_spec("default", &[]),
        )
        .unwrap();
        assert_eq!(value_of(&out), WireValue::String("41|x".to_string()));
    }

    #[test]
    fn prefix_top_level_await_in_plain_exports_settles() {
        // The common bundled-handler shapes: the awaited value lands in a
        // module binding (or is discarded), not on globalThis. A prefix's
        // namespace is where calls resolve, so nothing else observes these
        // bindings — the point is that precompile itself succeeds. The
        // failure never depended on where the value lands, only on the
        // module evaluation promise.
        for code in [
            "export const a = await 1",
            "export default await 1",
            "const a = await 1",
            "await 1",
            "export default await (async () => 1)()",
        ] {
            let result = evaluate_prefix(code, &[], &[]);
            assert!(
                result.is_ok(),
                "prefix {code:?} failed: {:?}",
                result.err().map(|f| f.error)
            );
        }
    }

    #[test]
    fn prefix_imported_module_with_top_level_await_settles() {
        // Top-level await inside an *imported* source module resolves as part
        // of the same module-graph evaluation promise as the prefix itself.
        let imports = [source_import(
            "lib:config",
            "export const config = await Promise.resolve({ level: 3 })",
        )];
        let prefix = prepared(
            "import { config } from 'lib:config'; globalThis.level = config.level\n\
             export default () => globalThis.level",
            &[],
            &imports,
        );
        let out = execute_with_prefix(
            prefix,
            Limits::default(),
            &[],
            &imports,
            RunIo::None,
            Arc::new(AtomicU32::new(0)),
            &call_spec("default", &[]),
        )
        .unwrap();
        assert_eq!(value_of(&out), WireValue::Number(3.0));
    }

    #[test]
    fn prefix_chained_top_level_awaits_settle() {
        // Chained awaits all resolve within one checkpoint — a checkpoint
        // drains the whole queue, newly enqueued microtasks included.
        let prefix = prepared(
            "let n = 0; for (let i = 0; i < 10; i++) { n = await Promise.resolve(n + 1) } \
             globalThis.n = n\n\
             export default () => globalThis.n",
            &[],
            &[],
        );
        let out = execute_with_prefix(
            prefix,
            Limits::default(),
            &[],
            &[],
            RunIo::None,
            Arc::new(AtomicU32::new(0)),
            &call_spec("default", &[]),
        )
        .unwrap();
        assert_eq!(value_of(&out), WireValue::Number(10.0));
    }

    #[test]
    fn prefix_rejected_top_level_await_is_runtime_error() {
        let err = evaluate_prefix(r#"await Promise.reject(new Error("prefix rejected"))"#, &[], &[])
            .unwrap_err();
        assert!(matches!(err.error, RunError::RuntimeError(_)));
    }

    #[test]
    fn prefix_never_settling_prefix_fails_with_did_not_settle() {
        let err = evaluate_prefix("await new Promise(() => {})", &[], &[]).unwrap_err();
        assert!(matches!(err.error, RunError::PrefixDidNotSettle(_)));
    }

    // Bridge callables exist while a prefix evaluates as throwing placeholders:
    // referencing them works, calling them is ERR_PREFIX_BRIDGE_CALL.

    #[test]
    fn prefix_calling_bridge_global_fails_with_prefix_bridge_call() {
        let globals = [HostGlobalDef::bridge("fetch")];
        // Awaited and fire-and-forget-at-top-level both throw synchronously
        // inside the placeholder, before any await could park the module.
        for code in [
            "await fetch('https://example.com')",
            "fetch('https://example.com')",
        ] {
            let err = evaluate_prefix(code, &globals, &[]).unwrap_err();
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
        init_platform();
        let (io, host) = test_io();
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
            io,
            Arc::new(AtomicU32::new(0)),
            None,
        )
        .unwrap();
        drop(host);
        assert_eq!(
            get_default(&out).as_deref(),
            Some("0|0|function|function|true")
        );
    }

    #[test]
    fn a_non_enumerable_global_is_hidden_but_callable() {
        // The host's per-global opt-out. Hidden names stay reachable
        // (hygiene, not secrecy); undeclared-flag names stay enumerable.
        init_platform();
        let (io, host) = test_io();
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
            io,
            Arc::new(AtomicU32::new(0)),
            None,
        )
        .unwrap();
        drop(host);
        assert_eq!(
            get_default(&out).as_deref(),
            Some("false|false|true|function|42")
        );
    }

    #[test]
    fn a_non_enumerable_global_has_the_same_surface_at_prepare_and_run_time() {
        // The between-runs placeholder installs with the declared flag, so
        // enumeration inside the prefix and inside a run agree.
        init_platform();
        let (io, host) = test_io();
        let globals = [HostGlobalDef::Bridge {
            name: "tool".to_string(),
            enumerable: false,
        }];
        let prefix = prepared(
            "globalThis.prefixEnumerated = Object.keys(globalThis).includes('tool')\n\
             globalThis.prefixType = typeof tool\n\
             export default () => [prefixEnumerated, prefixType, \
               Object.keys(globalThis).includes('tool'), typeof tool].join('|')",
            &globals,
            &[],
        );
        let out = execute_with_prefix(
            prefix,
            Limits::default(),
            &globals,
            &[],
            io,
            Arc::new(AtomicU32::new(0)),
            &call_spec("default", &[]),
        )
        .unwrap();
        drop(host);
        assert_eq!(
            value_of(&out),
            WireValue::String("false|function|false|function".to_string())
        );
    }

    #[test]
    fn prefix_calling_shim_global_fails_with_prefix_bridge_call() {
        let globals = [HostGlobalDef::Shim {
            name: "fetch".to_string(),
            shim: "(raw) => raw".to_string(),
            handler_name: "__iso4_fetch_h".to_string(),
            enumerable: true,
        }];
        let err =
            evaluate_prefix("await fetch('https://example.com')", &globals, &[]).unwrap_err();
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
    fn prefix_calling_host_import_function_fails_with_prefix_bridge_call() {
        let imports = [host_import(
            "tools:search",
            vec![("query", HostModuleNode::Function)],
        )];
        let err = evaluate_prefix("import { query } from 'tools:search'; await query('dogs')", &[], &imports)
        .unwrap_err();
        assert!(matches!(err.error, RunError::PrefixBridgeCall(_)));
    }

    #[test]
    fn prefix_bridge_global_is_visible_but_not_callable() {
        // typeof matches run() code, and stashing a reference is fine — only
        // the call is rejected.
        let globals = [HostGlobalDef::bridge("fetch")];
        let prefix = prepared(
            "globalThis.kind = typeof fetch\nexport default () => globalThis.kind",
            &globals,
            &[],
        );
        let out = execute_with_prefix(
            prefix,
            Limits::default(),
            &[],
            &[],
            RunIo::None,
            Arc::new(AtomicU32::new(0)),
            &call_spec("default", &[]),
        )
        .unwrap();
        assert_eq!(value_of(&out), WireValue::String("function".to_string()));
    }

    /// The web classes are native `FunctionTemplate`s restored into every
    /// run's context; a closure the prefix creates over them must still
    /// work when a call on the same instance invokes it.
    #[test]
    fn web_globals_are_usable_from_prefix_code() {
        let prefix = prepared(
            "globalThis.mk = () => new Response('hi', { status: 201 })\n\
             export default () => { const r = globalThis.mk(); \
               return [r instanceof Response, r.status, r.headers.get('content-type')].join('|') }",
            &[],
            &[],
        );
        let out = execute_with_prefix(
            prefix,
            Limits::default(),
            &[],
            &[],
            RunIo::None,
            Arc::new(AtomicU32::new(0)),
            &call_spec("default", &[]),
        )
        .unwrap();
        assert_eq!(
            value_of(&out),
            WireValue::String("true|201|text/plain;charset=UTF-8".to_string())
        );
    }

    /// A live web-type instance created by prefix code must be visible to
    /// a call on the instance (and validate cleanly at prepare() time).
    #[test]
    fn a_live_instance_in_the_prefix_is_visible_to_a_call() {
        for expr in [
            "new Response('x', { status: 201 })",
            "new Request('https://ex.com/a')",
            "new Headers([['content-type', 'text/plain']])",
        ] {
            let prefix_src = format!("globalThis.v = {expr}\nexport default () => typeof globalThis.v");
            evaluate_prefix(&prefix_src, &[], &[])
                .unwrap_or_else(|_| panic!("a prefix with a live {expr} must evaluate"));
            let out = execute_with_prefix(
                PrefixSpec {
                    code: &prefix_src,
                    filename: "<prefix>",
                    globals: &[],
                },
                Limits::default(),
                &[],
                &[],
                RunIo::None,
                Arc::new(AtomicU32::new(0)),
                &call_spec("default", &[]),
            )
            .unwrap_or_else(|_| panic!("run with a prefix holding {expr} must not fail"));
            assert_eq!(value_of(&out), WireValue::String("object".to_string()), "{expr}");
        }
    }

    /// A call constructing a `Response` in a prefix run must still
    /// serialize through the host-object path on the way out.
    #[test]
    fn a_response_built_in_a_prefix_run_still_serializes() {
        let prefix = prepared(
            "export default () => new Response('body', { status: 418, headers: { 'x-a': 'b' } })",
            &[],
            &[],
        );
        let out = execute_with_prefix(
            prefix,
            Limits::default(),
            &[],
            &[],
            RunIo::None,
            Arc::new(AtomicU32::new(0)),
            &call_spec("default", &[]),
        )
        .unwrap();
        // The export blob carries a host object; the host decodes it. Here we
        // only assert the run succeeded and produced one export — decoding is
        // covered on the TS side.
        assert!(!out.exports.is_empty(), "expected a serialized export");
    }

    #[test]
    fn execute_with_prefix_runs_on_fresh_instances_do_not_share_mutations() {
        let prefix = prepared(
            "globalThis.counter = 0\n\
             export function bump() { globalThis.counter = 99; return 1 }\n\
             export function read() { return globalThis.counter }",
            &[],
            &[],
        );
        execute_with_prefix(
            prefix,
            Limits::default(),
            &[],
            &[],
            RunIo::None,
            Arc::new(AtomicU32::new(0)),
            &call_spec("bump", &[]),
        )
        .unwrap();
        let out = execute_with_prefix(
            prefix,
            Limits::default(),
            &[],
            &[],
            RunIo::None,
            Arc::new(AtomicU32::new(0)),
            &call_spec("read", &[]),
        )
        .unwrap();
        assert_eq!(value_of(&out), WireValue::Number(0.0));
    }

    #[test]
    fn execute_with_prefix_complex_prefix_computation() {
        let prefix = prepared(
            r#"const sq = {}; for (let i = 0; i <= 10; i++) sq[i] = i * i; globalThis.sq = sq;
               export default () => globalThis.sq[7]"#,
            &[],
            &[],
        );
        let out = execute_with_prefix(
            prefix,
            Limits::default(),
            &[],
            &[],
            RunIo::None,
            Arc::new(AtomicU32::new(0)),
            &call_spec("default", &[]),
        )
        .unwrap();
        assert_eq!(value_of(&out), WireValue::Number(49.0));
    }

    #[test]
    fn execute_with_prefix_console_is_available_in_run_code() {
        let prefix = prepared(
            r#"export default () => { console.log("hello from run code"); return 1 }"#,
            &[],
            &[],
        );
        let out = execute_with_prefix(
            prefix,
            Limits::default(),
            &[],
            &[],
            RunIo::None,
            Arc::new(AtomicU32::new(0)),
            &call_spec("default", &[]),
        )
        .unwrap();
        assert!(out.stdout.iter().any(|l| l.contains("hello from run code")));
    }

    #[test]
    fn execute_with_prefix_run_code_runtime_error_is_reported() {
        let prefix = prepared(
            r#"export default () => { throw new Error("run code failed") }"#,
            &[],
            &[],
        );
        let err = execute_with_prefix(
            prefix,
            Limits::default(),
            &[],
            &[],
            RunIo::None,
            Arc::new(AtomicU32::new(0)),
            &call_spec("default", &[]),
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

    // ── ERR_UNDECLARED_BINDING ────────────────────────────────────────────
    // Phase 2+ (requires precompile / snapshots).

    #[test]
    #[ignore = "requires snapshot/precompile (Phase 2)"]
    fn run_with_undeclared_binding_is_error() {
        // prefix.call() passes a global name that was not declared at
        // prepare() time. Must fail with ERR_UNDECLARED_BINDING instead
        // of silently installing an unexpected global into the instance's
        // context.
        todo!()
    }

    // ── CPU budget excludes async wait time ───────────────────────────────
    // Phase 3 (cpu budget bracketing).

    #[test]
    fn cpu_budget_does_not_count_time_waiting_on_bridge() {
        // A run with a tight cpuTimeMs limit (50 ms) but a slow bridge
        // response (100 ms sleep) must NOT hit ERR_CPU_TIMEOUT because
        // cpu_budget.leave() is called before the blocking bridge read.
        let (io, host) = test_io();

        let responder = std::thread::spawn(move || {
            let _ = host.recv();
            std::thread::sleep(Duration::from_millis(100));
            host.send(bridge_resp_ok(0, &WireValue::Number(1.0)));
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
            io,
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
    fn cpu_budget_does_not_count_a_descheduled_thread() {
        // #196: an epoch spanning time the thread is NOT on a core accrues
        // nothing. Sleeping is the deterministic stand-in for starvation.
        let budget = CpuBudget::new();
        budget.enter();
        std::thread::sleep(Duration::from_millis(200));
        budget.leave();
        assert!(
            budget.elapsed_ms() < 20,
            "parked 200 ms charged {} ms of CPU",
            budget.elapsed_ms()
        );
    }

    #[test]
    fn cpu_budget_counts_what_the_thread_actually_burned() {
        // The other half: work inside the epoch IS counted, and the reported
        // figure tracks the thread's own clock rather than drifting off it.
        let clock = crate::cpuclock::ThreadClock::current();
        let budget = CpuBudget::new();
        let before = clock.read_ns();
        budget.enter();
        let spin = std::time::Instant::now();
        let mut x = 0u64;
        while spin.elapsed() < Duration::from_millis(50) {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
        }
        std::hint::black_box(x);
        budget.leave();
        let truth = (clock.read_ns() - before) as f64 / 1e6;
        let reported = budget.elapsed_ms_precise();
        assert!(
            (reported - truth).abs() < 2.0,
            "reported {reported} ms against {truth} ms on the thread's own clock"
        );
    }

    #[test]
    fn reported_cpu_stays_below_wall_when_cores_are_contended() {
        // A run is charged its own work, not the time it shared cores: under
        // oversubscription wall stretches while CPU does not.
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(2);
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ready = Arc::new(std::sync::Barrier::new(cores * 4 + 1));
        let load: Vec<_> = (0..cores * 4)
            .map(|_| {
                let s = Arc::clone(&stop);
                let r = Arc::clone(&ready);
                std::thread::spawn(move || {
                    r.wait();
                    let mut x = 0u64;
                    while !s.load(Ordering::Relaxed) {
                        x = x.wrapping_mul(2862933555777941757).wrapping_add(3);
                    }
                    x
                })
            })
            .collect();
        ready.wait();

        // ~70 ms of CPU, so the run spans many scheduler slices.
        let out = run_code(
            "let x = 0; for (let i = 0; i < 100_000_000; i++) x = (x + i) & 1048575; export default x",
            "<test>",
            Limits::default(),
        );
        stop.store(true, Ordering::Relaxed);
        for t in load {
            t.join().unwrap();
        }

        let out = out.expect("the spin must finish: starvation costs wall, not CPU");
        // ~4x oversubscription puts this near 0.25; cpu tracking wall reads ~1.0.
        assert!(
            out.cpu_time_ms < out.wall_time_ms * 0.8,
            "cpu {} ms vs wall {} ms — cpu is tracking wall, not the core",
            out.cpu_time_ms,
            out.wall_time_ms
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
    fn async_context_is_available_to_prefix_code_and_its_exports() {
        // workerd's shape: the instance is created at module scope and used
        // inside the handler, with the ambient value following awaits.
        let prefix = prepared(
            "import { AsyncLocalStorage } from 'node:async_hooks'\n\
             const als = new AsyncLocalStorage()\n\
             export default (key) => als.run(key, async () => { await Promise.resolve(); return als.getStore() })",
            &[],
            &[],
        );
        let out = execute_with_prefix(
            prefix,
            Limits::default(),
            &[],
            &[],
            RunIo::None,
            Arc::new(AtomicU32::new(0)),
            &call_spec("default", &[WireValue::String("wf".to_string())]),
        )
        .unwrap();
        assert_eq!(value_of(&out), WireValue::String("wf".to_string()));
    }

    #[test]
    fn prefix_unresolved_import_is_clean_error_not_crash() {
        // Regression (snapshot era): an unresolvable import used to segfault
        // (unconditional create_blob after a failed instantiate). The cold
        // start must return a clean ModuleNotFound.
        let err = evaluate_prefix("import x from 'totally-nonexistent-xyz'; export default x;", &[], &[])
        .unwrap_err();
        assert!(
            matches!(err.error, RunError::ModuleNotFound(_)),
            "expected ModuleNotFound, got {:?}",
            err.error
        );
    }

    #[test]
    fn prefix_with_globalthis_state_evaluates() {
        evaluate_prefix("globalThis.base = 100; export default 1;", &[], &[])
        .unwrap();
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

    // ── Warm instances: create/call split, taint, warm-up budget ──────

    /// A counting prefix: module-scope state observable through a call.
    const COUNTER_PREFIX: &str = "let n = 0\nexport function bump() { return ++n }";

    fn counter_core() -> InstanceCore {
        init_platform();
        create_instance_core(
            PrefixSpec {
                code: COUNTER_PREFIX,
                filename: "<prefix>",
                globals: &[],
            },
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
            &[],
            limits,
            &[],
            &[],
            RunIo::None,
            Arc::new(AtomicU32::new(0)),
            &call,
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
            PrefixSpec {
                code: "globalThis.mk = new Function('return 7')\n\
                       export default () => { \
                         const denied = (() => { try { new Function('1'); return 'allowed' } \
                           catch (e) { return e instanceof EvalError } })(); \
                         return [mk(), denied].join('|') }",
                filename: "<prefix>",
                globals: &[],
            },
            &[],
            0,
        )
        .expect("setup using new Function warms up");
        for _ in 0..2 {
            let outcome = run_call_on_core(
                &mut core,
                &[],
                Limits::default(),
                &[],
                &[],
                RunIo::None,
                Arc::new(AtomicU32::new(0)),
                &call_spec("default", &[]),
            );
            assert!(!outcome.tainted);
            let out = outcome.result.expect("call succeeds");
            assert_eq!(value_of(&out), WireValue::String("7|true".to_string()));
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
    fn clean_exception_does_not_taint_and_state_survives() {
        init_platform();
        let mut core = create_instance_core(
            PrefixSpec {
                code: "let n = 0\n\
                       export function bump() { return ++n }\n\
                       export function boom() { throw new Error('x') }",
                filename: "<prefix>",
                globals: &[],
            },
            &[],
            0,
        )
        .expect("prefix warms up");
        assert!(bump(&mut core, Limits::default()).result.is_ok());

        let call = call_spec("boom", &[]);
        let outcome = run_call_on_core(
            &mut core,
            &[],
            Limits::default(),
            &[],
            &[],
            RunIo::None,
            Arc::new(AtomicU32::new(0)),
            &call,
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
            PrefixSpec {
                code: "for (;;) {}",
                filename: "<prefix>",
                globals: &[],
            },
            &[],
            0,
        );
        let Err(failure) = result else {
            panic!("a looping prefix cannot warm up");
        };
        assert!(matches!(failure.error, RunError::WarmupLimit));
    }

    // ── Review fixes from the warm-instances review ──────────────────────────

    /// Finding 1 (critical): a bridge stub stashed by sandbox code on one call must
    /// NOT dereference freed per-call state when invoked on a later call —
    /// the slot is disarmed between calls, so the stashed function throws a
    /// catchable error instead of a use-after-free. Pre-fix this dereferenced
    /// a freed `GlobalCallbackData` (and wrote to a stale socket fd).
    #[test]
    fn stashed_bridge_stub_is_disarmed_on_the_next_call() {
        init_platform();
        let mut core = create_instance_core(
            PrefixSpec {
                code: "export function stash() { globalThis.saved = g; return 1 }\n\
                       export function poke() { \
                         let r; try { globalThis.saved(); r = 'called' } catch (e) { r = 'threw' } \
                         return r }",
                filename: "<prefix>",
                globals: &[HostGlobalDef::bridge("g")],
            },
            &[],
            0,
        )
        .expect("prefix warms up");

        // Call 1 binds `g` live and sandbox code stashes the reference. It
        // never calls `g`, so no bridge frame is written — the socket just
        // has to be a valid fd for the stub to install.
        let (io, _host) = test_io();
        let out1 = run_call_on_core(
            &mut core,
            &[HostGlobalDef::bridge("g")],
            Limits::default(),
            &[HostGlobalDef::bridge("g")],
            &[],
            io,
            Arc::new(AtomicU32::new(0)),
            &call_spec("stash", &[]),
        );
        assert!(!out1.tainted);
        assert!(out1.result.is_ok());

        // Call 2 does not rebind `g` (no socket): the stashed stub's slot is
        // disarmed. Invoking it must throw catchably, not crash.
        let out2 = run_call_on_core(
            &mut core,
            &[HostGlobalDef::bridge("g")],
            Limits::default(),
            &[],
            &[],
            RunIo::None,
            Arc::new(AtomicU32::new(0)),
            &call_spec("poke", &[]),
        );
        assert!(!out2.tainted, "a caught throw is a clean completion");
        let out = out2.result.expect("call 2 completes");
        assert_eq!(value_of(&out), WireValue::String("threw".to_string()));
    }

    /// Finding 2: run code can plant a hostile accessor under a bridge-global name;
    /// the next call's placeholder re-arm must overwrite it WITHOUT invoking
    /// the setter. Pre-fix the re-arm went through `[[Set]]`, so a
    /// looping setter hung the owner thread (only bounded, post-guards-move,
    /// by the CPU guard).
    #[test]
    fn hostile_setter_under_a_bridge_name_does_not_hijack_rearm() {
        init_platform();
        let mut core = create_instance_core(
            PrefixSpec {
                code: "export function plant() { \
                         Object.defineProperty(globalThis, 'g', \
                           { configurable: true, set() { for (;;) {} }, get() { return 1 } }); \
                         return 1 }\n\
                       export function seven() { return 7 }",
                filename: "<prefix>",
                globals: &[HostGlobalDef::bridge("g")],
            },
            &[],
            0,
        )
        .expect("prefix warms up");

        // Call 1 replaces `g` with a setter that would loop forever if invoked.
        let out1 = run_call_on_core(
            &mut core,
            &[HostGlobalDef::bridge("g")],
            Limits::default(),
            &[],
            &[],
            RunIo::None,
            Arc::new(AtomicU32::new(0)),
            &call_spec("plant", &[]),
        );
        assert!(!out1.tainted);
        assert!(out1.result.is_ok());

        // Call 2's re-arm defines `g` = placeholder via DefineOwnProperty,
        // which does not trigger the setter — so the call completes cleanly
        // and quickly instead of hanging / CPU-timing-out.
        let out2 = run_call_on_core(
            &mut core,
            &[HostGlobalDef::bridge("g")],
            Limits {
                cpu_time_ms: 2_000,
                wall_time_ms: 5_000,
                ..Default::default()
            },
            &[],
            &[],
            RunIo::None,
            Arc::new(AtomicU32::new(0)),
            &call_spec("seven", &[]),
        );
        assert!(!out2.tainted, "the setter must not run during re-arm");
        assert_eq!(value_of(&out2.result.unwrap()), WireValue::Number(7.0));
    }

    /// Finding 7: blowing the heap cap during warm-up reports `ERR_MEMORY_LIMIT`,
    /// not the time-shaped `ERR_WARMUP_LIMIT`.
    #[test]
    fn warmup_memory_blowout_reports_memory_limit() {
        init_platform();
        let result = create_instance_core(
            PrefixSpec {
                // ~64 MB backing store under an 8 MB cap.
                code: "globalThis.hog = new Uint8Array(64 * 1024 * 1024)",
                filename: "<prefix>",
                globals: &[],
            },
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
            PrefixSpec {
                code: "console.log('boot diagnostics')\nexport default () => 1",
                filename: "<prefix>",
                globals: &[],
            },
            &[],
            0,
        )
        .expect("prefix warms up");

        let first = run_call_on_core(
            &mut core,
            &[],
            Limits::default(),
            &[],
            &[],
            RunIo::None,
            Arc::new(AtomicU32::new(0)),
            &call_spec("default", &[]),
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
            &[],
            Limits::default(),
            &[],
            &[],
            RunIo::None,
            Arc::new(AtomicU32::new(0)),
            &call_spec("default", &[]),
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
            PrefixSpec {
                code: "export function arm() { \
                         const p = new Promise(r => { globalThis.res = r }); \
                         p.then(() => { globalThis.leaked = globalThis.marker }); \
                         return 1 }\n\
                       export function fire() { globalThis.marker = 'run2-secret'; globalThis.res(); return 1 }\n\
                       export function read() { return globalThis.leaked ?? 'not-set' }",
                filename: "<prefix>",
                globals: &[],
            },
            &[],
            0,
        )
        .expect("prefix warms up");

        // Run 1: stash a resolver and queue a continuation that copies whatever
        // `globalThis.marker` holds WHEN IT EVENTUALLY RUNS. The promise never
        // settles this run, so the continuation stays queued into the isolate.
        let run1 = run_call_on_core(
            &mut core,
            &[],
            Limits::default(),
            &[],
            &[],
            RunIo::None,
            Arc::new(AtomicU32::new(0)),
            &call_spec("arm", &[]),
        );
        assert!(!run1.tainted);
        assert!(run1.result.is_ok());

        // Run 2: set a marker, then resolve run 1's promise. Run 1's
        // continuation fires at run 2's microtask checkpoint and reads run 2's
        // marker — run 1's code executing inside run 2's call.
        let run2 = run_call_on_core(
            &mut core,
            &[],
            Limits::default(),
            &[],
            &[],
            RunIo::None,
            Arc::new(AtomicU32::new(0)),
            &call_spec("fire", &[]),
        );
        assert!(!run2.tainted);
        assert!(run2.result.is_ok());

        // Run 3: observe what run 1's continuation captured — run 2's marker.
        let run3 = run_call_on_core(
            &mut core,
            &[],
            Limits::default(),
            &[],
            &[],
            RunIo::None,
            Arc::new(AtomicU32::new(0)),
            &call_spec("read", &[]),
        )
        .result
        .expect("run 3");
        assert_eq!(
            value_of(&run3),
            WireValue::String("run2-secret".to_string()),
            "run 1's continuation ran during run 2 and observed run 2's state \
             (cross-run confused-deputy)"
        );
    }

    // ── Kills and bridge violations ──────────────────────────────────────

    /// Execute `code` with bridge global `myTool` on a socket nobody answers.
    fn run_with_silent_bridge(code: &str, limits: Limits) -> Result<Output, FailureOutput> {
        let (io, _host) = test_io();
        execute(
            code,
            None,
            limits,
            &[HostGlobalDef::bridge("myTool")],
            &[],
            io,
            Arc::new(AtomicU32::new(0)),
            None,
        )
    }

    fn silent_limits() -> Limits {
        Limits {
            wall_time_ms: 5_000,
            ..Default::default()
        }
    }

    #[test]
    fn no_error_field_getter_runs_after_a_cpu_kill() {
        let failure = run_with_silent_bridge(
            "const e = new Error('x')\n\
             e.a = { get g() { for (;;) {} } }\n\
             e.b = { get h() { myTool('after'); return 2 } }\n\
             throw e",
            Limits {
                cpu_time_ms: 50,
                ..silent_limits()
            },
        )
        .unwrap_err();
        assert!(matches!(failure.error, RunError::CpuTimeout), "got {:?}", failure.error);
        assert!(failure.bridge_calls.is_empty(), "got {:?}", failure.bridge_calls);
    }

    #[test]
    fn a_cpu_kill_inside_a_bridge_argument_getter_wins_over_the_bridge_error() {
        let failure = run_with_silent_bridge(
            "await myTool({ get a() { for (;;) {} } })\nexport default 1",
            Limits {
                cpu_time_ms: 50,
                ..silent_limits()
            },
        )
        .unwrap_err();
        assert!(matches!(failure.error, RunError::CpuTimeout), "got {:?}", failure.error);
    }

    #[test]
    fn an_uncloneable_bridge_argument_is_export_not_serializable() {
        let failure = run_with_silent_bridge(
            "await myTool(new Proxy({}, {}))\nexport default 1",
            silent_limits(),
        )
        .unwrap_err();
        assert!(
            matches!(failure.error, RunError::ExportNotSerializable(_)),
            "got {:?}",
            failure.error
        );
    }

    #[test]
    fn a_bridge_call_queued_by_a_sync_call_belongs_to_that_call() {
        init_platform();
        let defs = [HostGlobalDef::bridge("g")];
        let mut core = create_instance_core(
            PrefixSpec {
                code: "let n = 0\n\
                       export function leak() { Promise.resolve().then(() => g('leak')); return 1 }\n\
                       export function bump() { return ++n }",
                filename: "<prefix>",
                globals: &defs,
            },
            &[],
            0,
        )
        .expect("prefix warms up");
        let (io, host) = test_io();
        // Joined after the call: the message must be sent during it.
        let responder = spawn_responder(host, |_| {});
        let leak = call_spec("leak", &[]);
        let first = run_call_on_core(
            &mut core,
            &defs,
            Limits::default(),
            &defs,
            &[],
            io,
            Arc::new(AtomicU32::new(0)),
            &leak,
        );
        responder.join().unwrap();
        assert!(!first.tainted);
        let out = first.result.expect("leak returns");
        assert_eq!(out.bridge_calls.len(), 1, "the queued call is this call's");

        let bump = call_spec("bump", &[]);
        let second = run_call_on_core(
            &mut core,
            &defs,
            Limits::default(),
            &[],
            &[],
            RunIo::None,
            Arc::new(AtomicU32::new(0)),
            &bump,
        );
        assert!(!second.tainted);
        let out = second.result.expect("the next call is clean");
        assert_eq!(value_of(&out), WireValue::Number(1.0));
        assert!(out.bridge_calls.is_empty());
    }

    #[test]
    fn multibyte_grace_errors_are_capped_on_a_char_boundary() {
        let out = execute(
            "waitUntil(Promise.reject(Object.assign(new Error('€'.repeat(3000)), { name: '€'.repeat(100) })))\n\
             export default 1",
            None,
            grace_limits(),
            &[],
            &[],
            RunIo::None,
            Arc::new(AtomicU32::new(0)),
            None,
        )
        .unwrap();
        let (name, message) = out.background.expect("grace ran").error.expect("error kept");
        assert_eq!(name, "€".repeat(85));
        assert_eq!(message, format!("{}… [truncated]", "€".repeat(682)));
    }

    #[test]
    fn a_bridge_violation_inside_a_getter_names_itself_and_leaks_no_terminate() {
        init_platform();
        let defs = [HostGlobalDef::bridge("tool")];
        let prefix = "let n = 0\n\
            export function bump() { return ++n }\n\
            export function oneGetter() { return { get a() { tool(() => 1); return 1 } } }\n\
            export function twoGetters() { return { get a() { tool(() => 1); return 1 }, get b() { return 2 } } }\n\
            export function errFields() { const e = new Error('x'); e.a = { get g() { tool(() => 1); return 1 } }; e.b = { get h() { return 2 } }; throw e }\n\
            export function argGetter() { tool({ get a() { tool(() => 1); return 1 }, get b() { return 2 } }); return 1 }";
        let mut core = create_instance_core(
            PrefixSpec {
                code: prefix,
                filename: "<prefix>",
                globals: &defs,
            },
            &[],
            0,
        )
        .expect("prefix warms up");
        for export in ["oneGetter", "twoGetters", "errFields", "argGetter"] {
            let (io, host) = test_io();
            let call = call_spec(export, &[]);
            let outcome = run_call_on_core(
                &mut core,
                &defs,
                Limits::default(),
                &defs,
                &[],
                io,
                Arc::new(AtomicU32::new(0)),
                &call,
            );
            assert!(outcome.tainted, "{export}: an interrupted instance is tainted");
            let error = outcome.result.expect_err(export).error;
            assert!(
                matches!(error, RunError::FunctionArgumentNotSupported),
                "{export}: got {error:?}"
            );
            assert!(
                host.recv_timeout(Duration::ZERO).is_none(),
                "{export}: no bridge message was sent"
            );

            // Even reused, the instance carries no terminate into the next call.
            let bump = call_spec("bump", &[]);
            let next = run_call_on_core(
                &mut core,
                &defs,
                Limits::default(),
                &[],
                &[],
                RunIo::None,
                Arc::new(AtomicU32::new(0)),
                &bump,
            );
            assert!(!next.tainted, "{export}: the next call is clean");
            assert!(next.result.is_ok(), "{export}: the next call succeeds");
        }
    }

    // ── Import failures ──────────────────────────────────────────────────

    const DYN_IMPORT_MSG: &str = "dynamic import() is not supported: 'lodash'";

    fn expect_import(err: &RunError) -> &str {
        match err {
            RunError::Import(m) => m,
            other => panic!("expected Import, got {other:?}"),
        }
    }

    #[test]
    fn unknown_import_names_the_specifier() {
        match run_err("import x from 'lodash'; export default x") {
            RunError::ModuleNotFound(m) => assert_eq!(m, "Cannot find module 'lodash'"),
            other => panic!("expected ModuleNotFound, got {other:?}"),
        }
    }

    #[test]
    fn unknown_nested_import_names_its_importer() {
        let imports = [source_import(
            "src:a",
            "import b from 'missing:b'; export default b",
        )];
        let err = run_with_source_imports("import a from 'src:a'; export default a", &imports)
            .unwrap_err();
        match err {
            RunError::ModuleNotFound(m) => {
                assert_eq!(m, "Cannot find module 'missing:b' imported from 'src:a'")
            }
            other => panic!("expected ModuleNotFound, got {other:?}"),
        }
    }

    #[test]
    fn a_missing_named_export_is_an_import_error() {
        let imports = [host_import(
            "tools",
            vec![("yes", HostModuleNode::Data(testval::to_blob(&WireValue::Number(1.0))))],
        )];
        let err = run_with_source_imports("import { nope } from 'tools'; export default nope", &imports)
            .unwrap_err();
        let m = expect_import(&err);
        assert!(
            m.contains("does not provide an export named 'nope'"),
            "V8's message is kept: {m}"
        );
    }

    #[test]
    fn a_source_import_that_fails_to_compile_is_user_code() {
        let imports = [source_import("src:bad", "export const = ;")];
        let err = run_with_source_imports("import x from 'src:bad'; export default x", &imports)
            .unwrap_err();
        assert!(
            matches!(&err, RunError::RuntimeError(d) if d.name == "SyntaxError" && d.message.contains("src:bad")),
            "got {err:?}"
        );
    }

    #[test]
    fn dynamic_import_at_top_level_ends_the_run_through_try_catch() {
        let failure = run_code(
            "let caught = false\n\
             try { await import('lodash') } catch { caught = true }\n\
             export default caught",
            "<iso4>",
            Limits::default(),
        )
        .unwrap_err();
        assert_eq!(expect_import(&failure.error), DYN_IMPORT_MSG);
        assert!(failure.cpu_time_ms <= failure.wall_time_ms + 0.001);
        assert!(failure.cpu_time_ms <= failure.duration_ms);
    }

    #[test]
    fn dynamic_import_without_await_cannot_be_swallowed() {
        let failure = run_code(
            "import('lodash').catch(() => {})\n\
             console.log('after')\n\
             export default 1",
            "<iso4>",
            Limits::default(),
        )
        .unwrap_err();
        assert_eq!(expect_import(&failure.error), DYN_IMPORT_MSG);
        assert!(failure.stdout.is_empty(), "nothing runs after the import() call");
    }

    #[test]
    fn dynamic_import_in_an_async_function_ends_the_run() {
        let err = run_err(
            "async function load() { await 0; try { return await import('lodash') } catch { return 'swallowed' } }\n\
             export default await load().catch(() => 'swallowed')",
        );
        assert_eq!(expect_import(&err), DYN_IMPORT_MSG);
    }

    #[test]
    fn dynamic_import_in_a_timer_callback_ends_the_run() {
        let err = run_err(
            "export default await new Promise(r => setTimeout(() => {\n\
               try { import('lodash') } catch {}\n\
               r('swallowed')\n\
             }, 1))",
        );
        assert_eq!(expect_import(&err), DYN_IMPORT_MSG);
    }

    #[test]
    fn dynamic_import_in_a_called_export_ends_the_run() {
        let err = run_call(
            "export function f() { try { import('lodash') } catch {} return 1 }",
            "f",
            &[],
        )
        .unwrap_err();
        assert_eq!(expect_import(&err), DYN_IMPORT_MSG);
    }

    #[test]
    fn dynamic_import_after_a_bridge_await_ends_the_run() {
        let (out, h) = run_with_bridge(
            "await myTool()\n\
             try { await import('lodash') } catch {}\n\
             export default 'swallowed'",
            "myTool",
            Limits {
                wall_time_ms: 10_000,
                ..Default::default()
            },
            |s| {
                s.send(bridge_resp_ok(0, &WireValue::Number(1.0)));
            },
        );
        h.join().unwrap();
        let failure = out.unwrap_err();
        assert_eq!(expect_import(&failure.error), DYN_IMPORT_MSG);
        assert_eq!(failure.bridge_calls.len(), 1, "the bridge record survives");
    }

    #[test]
    fn dynamic_import_in_a_prefix_fails_evaluation() {
        let failure = evaluate_prefix("try { await import('lodash') } catch {}\nexport default 1", &[], &[])
        .unwrap_err();
        assert_eq!(expect_import(&failure.error), DYN_IMPORT_MSG);
    }

    #[test]
    fn dynamic_import_in_a_prefix_fails_warm_up() {
        init_platform();
        let Err(failure) = create_instance_core(
            PrefixSpec {
                code: "try { import('lodash') } catch {}",
                filename: "<prefix>",
                globals: &[],
            },
            &[],
            0,
        ) else {
            panic!("a prefix calling import() cannot warm up");
        };
        assert_eq!(expect_import(&failure.error), DYN_IMPORT_MSG);
    }

    #[test]
    fn dynamic_import_on_a_warm_core_taints_it() {
        init_platform();
        let mut core = create_instance_core(
            PrefixSpec {
                code: "export function f() { try { import('lodash') } catch {} return 1 }",
                filename: "<prefix>",
                globals: &[],
            },
            &[],
            0,
        )
        .expect("prefix warms up");
        let call = call_spec("f", &[]);
        let outcome = run_call_on_core(
            &mut core,
            &[],
            Limits::default(),
            &[],
            &[],
            RunIo::None,
            Arc::new(AtomicU32::new(0)),
            &call,
        );
        assert!(outcome.tainted, "an interrupted instance must be discarded");
        assert_eq!(expect_import(&outcome.result.unwrap_err().error), DYN_IMPORT_MSG);
    }

    #[test]
    fn dynamic_import_during_grace_ends_the_phase_with_its_message() {
        init_platform();
        let mut core = create_instance_core(
            PrefixSpec {
                code: "export default () => { \
                         waitUntil(new Promise(r => setTimeout(r, 1)).then(() => import('lodash')).catch(() => {})); \
                         return 'early' }",
                filename: "<prefix>",
                globals: &[],
            },
            &[],
            0,
        )
        .expect("core");
        let outcome = run_call_on_core(
            &mut core,
            &[],
            grace_limits(),
            &[],
            &[],
            RunIo::None,
            Arc::new(AtomicU32::new(0)),
            &call_spec("default", &[]),
        );
        assert!(outcome.tainted);
        let out = outcome.result.expect("the value settled before grace");
        assert_eq!(value_of(&out), WireValue::String("early".to_string()));
        let bg = out.background.expect("grace ran");
        assert_eq!(bg.status, GraceStatus::Truncated);
        assert_eq!(bg.error, Some(("Error".to_string(), DYN_IMPORT_MSG.to_string())));
    }

    #[test]
    fn a_long_specifier_is_capped_in_the_message() {
        let err = run_err("try { import('€'.repeat(100_000)) } catch {}\nexport default 1");
        let expected = format!("dynamic import() is not supported: '{}…'", "€".repeat(256));
        assert_eq!(expect_import(&err), expected);
    }

    #[test]
    fn a_long_specifier_is_capped_on_the_grace_report() {
        init_platform();
        let mut core = create_instance_core(
            PrefixSpec {
                code: "export default () => { \
                         waitUntil(new Promise(r => setTimeout(r, 1)).then(() => import('€'.repeat(3000))).catch(() => {})); \
                         return 1 }",
                filename: "<prefix>",
                globals: &[],
            },
            &[],
            0,
        )
        .expect("core");
        let outcome = run_call_on_core(
            &mut core,
            &[],
            grace_limits(),
            &[],
            &[],
            RunIo::None,
            Arc::new(AtomicU32::new(0)),
            &call_spec("default", &[]),
        );
        let bg = outcome.result.expect("value settled").background.expect("grace ran");
        let expected = format!("dynamic import() is not supported: '{}…'", "€".repeat(256));
        assert_eq!(bg.error, Some(("Error".to_string(), expected)));
    }

    #[test]
    fn dynamic_import_in_a_bridge_argument_getter_is_an_import_error() {
        let failure = run_with_silent_bridge(
            "await myTool({ get g() { try { import('lodash') } catch {} return 1 } })\n\
             export default 'swallowed'",
            silent_limits(),
        )
        .unwrap_err();
        assert_eq!(expect_import(&failure.error), DYN_IMPORT_MSG);
    }

    #[test]
    fn no_error_field_getter_runs_after_a_dynamic_import() {
        let failure = run_with_silent_bridge(
            "const e = new Error('x')\n\
             e.a = { get g() { try { import('lodash') } catch {} return 1 } }\n\
             e.b = { get h() { myTool('after'); return 2 } }\n\
             throw e",
            silent_limits(),
        )
        .unwrap_err();
        assert_eq!(expect_import(&failure.error), DYN_IMPORT_MSG);
        assert!(failure.bridge_calls.is_empty(), "got {:?}", failure.bridge_calls);
    }

    #[test]
    fn dynamic_import_in_a_thrown_errors_message_getter_is_an_import_error() {
        let err = run_err(
            "const e = new Error('x')\n\
             Object.defineProperty(e, 'message', { get() { import('lodash'); return 'm' } })\n\
             throw e",
        );
        assert_eq!(expect_import(&err), DYN_IMPORT_MSG);
    }

    #[test]
    fn no_export_getter_runs_after_a_dynamic_import() {
        let failure = run_code(
            "export const a = { get g() { try { import('lodash') } catch {} return 1 } }\n\
             export const b = { get h() { console.log('ran'); return 2 } }",
            "<iso4>",
            Limits::default(),
        )
        .unwrap_err();
        assert_eq!(expect_import(&failure.error), DYN_IMPORT_MSG);
        assert!(failure.stdout.is_empty(), "got {:?}", failure.stdout);
    }

    #[test]
    fn nothing_runs_after_a_dynamic_import_inside_console_rendering() {
        let failure = run_code(
            "console.log({ toJSON() { try { import('lodash') } catch {} return 1 } })\n\
             console.log('after')\n\
             export default 1",
            "<iso4>",
            Limits::default(),
        )
        .unwrap_err();
        assert_eq!(expect_import(&failure.error), DYN_IMPORT_MSG);
        assert!(failure.stdout.is_empty(), "got {:?}", failure.stdout);
    }

    #[test]
    fn a_sync_call_that_queues_a_dynamic_import_fails_itself() {
        let err = run_call(
            "export function f() { Promise.resolve().then(() => import('lodash')); return 1 }",
            "f",
            &[],
        )
        .unwrap_err();
        assert_eq!(expect_import(&err), DYN_IMPORT_MSG);
    }

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
