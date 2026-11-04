//! Isolate creation and its per-instance state: the platform, limits, the
//! termination reason, the ArrayBuffer budget, the CPU budget, and the warm
//! `InstanceCore` with its idle settle.

use std::ffi::c_void;
use std::sync::{Arc, Mutex, Once, OnceLock};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use crate::errors::failure;
use crate::types;
use crate::types::ImportBinding;
use crate::v8::bridge::StubSlots;
use crate::v8::guard::{GuardTarget, InstanceGuard};
use crate::v8::modules::{evaluate_prefix_module, host_import_meta_callback};
use crate::v8::oom_label::oom_error_cb;
use crate::v8::runtable::RunTable;
use crate::v8::{FailureOutput, PrefixSpec, RunError, WARMUP_CPU_MS, WARMUP_WALL_MS};

static INIT: Once = Once::new();

#[derive(Default)]
pub(crate) struct LogBuffers {
    pub(crate) stdout: Vec<String>,
    pub(crate) stderr: Vec<String>,
    /// Running total of bytes across all stdout lines (excluding newlines).
    pub(super) stdout_bytes: usize,
    /// Running total of bytes across all stderr lines (excluding newlines).
    pub(super) stderr_bytes: usize,
    /// Cap for stdout. Zero = no limit.
    pub(super) max_stdout_bytes: u32,
    /// Cap for stderr. Zero = no limit.
    pub(super) max_stderr_bytes: u32,
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
/// a hard abort's mid-turn kill ([`crate::v8::GuardCtl::abort_executing`]).
///
/// Stored in a [`ReasonCell`] - first write wins, subsequent writes are
/// no-ops. Absence of a fired guard is represented naturally by
/// `ReasonCell::get()` returning `None`. The kill writes its reason BEFORE
/// `terminate_execution`, so detection never rides on V8's transient
/// terminate flag (which a microtask checkpoint can consume silently).
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum TerminationReason {
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
pub(crate) struct ReasonCell {
    reason: Mutex<Option<TerminationReason>>,
    /// A fatal bridge violation terminated without a reason; no JS check may
    /// ever consume that terminate, so the taint verdict reads this instead.
    pub(super) bare_terminate: AtomicBool,
}

impl ReasonCell {
    pub(super) fn new() -> Self {
        Self {
            reason: Mutex::new(None),
            bare_terminate: AtomicBool::new(false),
        }
    }

    /// First write wins; later writes are no-ops, matching `OnceLock::set`.
    pub(super) fn set(&self, reason: TerminationReason) {
        let mut slot = self.reason.lock().unwrap_or_else(|p| p.into_inner());
        slot.get_or_insert(reason);
    }

    pub(crate) fn get(&self) -> Option<TerminationReason> {
        self.reason.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    pub(super) fn bare_terminate(&self) -> bool {
        self.bare_terminate.load(Ordering::Relaxed)
    }

    /// Clear the slot for the next call on a reused isolate. Only safe while
    /// no JS is executing and no guards are armed — i.e. between calls.
    pub(super) fn reset(&self) {
        *self.reason.lock().unwrap_or_else(|p| p.into_inner()) = None;
        self.bare_terminate.store(false, Ordering::Relaxed);
    }
}

/// Heap data passed to [`near_heap_limit_cb`] as a raw pointer.
///
/// Lives on the heap for the duration of the run (see safety note on the
/// callback). Declaring it after `isolate` in [`crate::v8::runloop::run_module`] means it drops
/// before the isolate, which is safe because V8 never invokes near-heap
/// callbacks during `Isolate::Dispose()`.
pub(super) struct NearHeapData {
    pub(super) handle: v8::IsolateHandle,
    pub(super) reason: Arc<ReasonCell>,
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

pub(super) struct BudgetAllocState {
    /// Currently allocated ArrayBuffer bytes (may briefly exceed budget while
    /// terminate_execution propagates to the next JS safepoint).
    pub(super) used: AtomicUsize,
    /// Hard cap in bytes.  Always > 0 when the allocator is active.
    pub(super) budget: usize,
    /// Set once, immediately after `Isolate::new` returns.
    pub(super) handle: OnceLock<v8::IsolateHandle>,
    /// Shared with wall / cpu guards — first writer wins.
    pub(super) reason: Arc<ReasonCell>,
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
pub(super) static BUDGET_ALLOC_VTABLE: v8::RustAllocatorVtable<BudgetAllocState> = v8::RustAllocatorVtable {
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
pub(super) extern "C" fn near_heap_limit_cb(
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

/// Wasm is removed from the sandbox for now (DESIGN §7.5): the global is
/// deleted at every boot (`webtypes::delete_removed_globals`); this
/// isolate-wide denial backstops codegen.
pub(super) unsafe extern "C" fn deny_wasm_codegen(
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
pub(super) fn install_dynamic_import(isolate: &mut v8::OwnedIsolate, reason: &Arc<ReasonCell>) {
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
/// The epoch reads the executing thread's CPU clock, not the wall clock:
/// an instance owns its thread, so wall would charge a run for time it sat
/// descheduled behind co-resident runs. V8's background threads (GC,
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

/// Everything that lives as long as an isolate: the isolate itself, its
/// context, the once-per-instance installs (console, web runtime, async
/// context), and the evaluated prefix module. One-off runs build the same
/// state inline per run (`run_module_inner`); warm instances retain an
/// `InstanceCore` across calls on a dedicated owner thread.
pub struct InstanceCore {
    // Field order is drop order: the Globals must die before the isolate.
    pub(super) context: v8::Global<v8::Context>,
    pub(super) prefix_module: v8::Global<v8::Module>,
    pub(super) isolate: v8::OwnedIsolate,
    /// Per-run state table. The once-per-instance native installs (console,
    /// web runtime, waitUntil, bridge stubs) hold a raw pointer into this
    /// Box (stable address) and route by the attributed run token.
    pub(super) run_table: Box<RunTable>,
    /// Termination-reason slot shared with the near-heap callback and the
    /// ArrayBuffer allocator, which are registered once per isolate. Reset
    /// per call — the reason a [`ReasonCell`] replaced the old `OnceLock`.
    pub(super) reason: Arc<ReasonCell>,
    /// Keeps the identity of the allocator state visible to the core. The
    /// allocator holds its own Arc reference (via `Arc::into_raw`), and its
    /// `used` counter deliberately accumulates across calls: the heap cap is
    /// per instance, not per call.
    #[allow(dead_code)]
    alloc_state: Option<Arc<BudgetAllocState>>,
    /// Owns the near-heap callback's data for the isolate's lifetime.
    #[allow(dead_code)]
    near_heap: Option<Box<NearHeapData>>,
    /// Bridge-stub identity slots (see [`super::bridge::StubSlot`]): the pointees of every
    /// stub `Function` ever created in this context. Per-run bindings live
    /// in the run table; these Boxes only carry the name + table pointer.
    pub(super) stub_slots: StubSlots,
    /// The instance's watchdog registration (see [`InstanceGuard`]): warm-up
    /// and every turn of every call publish their budgets through it.
    pub(super) guard: InstanceGuard,
    /// True until the first call: warm-up console output (the prefix's
    /// `console.log`s) is sitting in the table's `setup_logs` and is
    /// delivered on the cold-start call's result instead of being dropped.
    pub(super) warmup_logs_pending: bool,
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

pub(super) fn heap_used(isolate: &mut v8::OwnedIsolate) -> u64 {
    isolate.get_heap_statistics().used_heap_size() as u64
}

/// One full collection when an instance goes idle — V8's own memory reducer
/// never runs here: nothing pumps the platform loop.
pub(super) fn settle_heap(isolate: &mut v8::OwnedIsolate) {
    isolate.low_memory_notification();
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU32;

    use super::*;
    use crate::testval::TestValue as WireValue;
    use crate::types::HostGlobalDef;
    use crate::v8::testsupport::{bridge_resp_ok, call_spec, run_code, test_io, value_of};
    use crate::v8::{CallOutcome, RunIo, execute, run_call_on_core};

    // ── Memory limits ─────────────────────────────────────────────────────

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
        // An epoch spanning time the thread is NOT on a core accrues
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
}
