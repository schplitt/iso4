//! V8 isolate management and JavaScript execution.
//!
//! Owns everything V8-related: platform init, isolate creation, compilation,
//! evaluation, result extraction, console capture, and limit enforcement.

use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::OnceLock;
use std::sync::{Arc, Mutex};
use std::sync::Once;
use std::time::{Duration, Instant};

use crossbeam_channel::RecvTimeoutError;

use crate::ipc;
use crate::wire::{self, WireValue};

static INIT: Once = Once::new();

#[derive(Default)]
struct LogBuffers {
    stdout: Vec<String>,
    stderr: Vec<String>,
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
    pub cpu_time_ms:  u32,
    pub memory_mb:    u32, // reserved - enforced in Phase 8
}


/// Termination reason set by the first limit-guard thread to fire.
///
/// Stored in a `OnceLock` - first write wins, subsequent writes are no-ops.
/// The two-variant enum means absence of timeout is represented naturally
/// by `OnceLock::get()` returning `None`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TerminationReason {
    Wall,
    Cpu,
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
    epoch_start:    Mutex<Option<Instant>>,
}

impl CpuBudget {
    pub fn new() -> Self {
        Self {
            accumulated_ns: AtomicU64::new(0),
            epoch_start:    Mutex::new(None),
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
            self.accumulated_ns.fetch_add(
                t.elapsed().as_nanos() as u64,
                Ordering::Relaxed,
            );
        }
    }

    /// Total accumulated CPU time in milliseconds.
    pub fn elapsed_ms(&self) -> u64 {
        let base   = self.accumulated_ns.load(Ordering::Relaxed);
        let active = self.epoch_start
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .map(|t| t.elapsed().as_nanos() as u64)
            .unwrap_or(0);
        (base + active) / 1_000_000
    }
}

// ── Output types ─────────────────────────────────────────────────────────────

/// The result of a successful JavaScript execution.
#[derive(Debug)]
pub struct Output {
    /// All exports as a flat `WireValue::Object`.
    /// The `default` export (if any) appears as the `"default"` key alongside
    /// named exports. An empty module produces `WireValue::Object(vec![])`.
    pub exports: WireValue,

    /// Lines written to console.log / console.debug / console.info.
    pub stdout: Vec<String>,

    /// Lines written to console.warn / console.error.
    pub stderr: Vec<String>,

    /// Wall-clock time from start of execution to result, in milliseconds.
    pub duration_ms: u64,
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
    pub duration_ms: u64,
}

/// All the ways an execution can fail.
#[derive(Debug)]
pub enum RunError {
    /// Payload bytes are not valid UTF-8.
    InvalidPayload(String),
    /// JS syntax error or compile-time error.
    CompileError(String),
    /// Uncaught exception thrown during execution.
    RuntimeError {
        message: String,
        stack: Option<String>,
    },
    /// `import` specifier not found in the host imports map.
    ModuleNotFound(String),
    /// An export value is a function or an unresolved Promise.
    ExportNotSerializable(String),
    /// Active JS execution time exceeded `limits.cpuTimeMs`.
    CpuTimeout,
    /// Total wall-clock time exceeded `limits.wallTimeMs`.
    WallTimeout,
    /// V8 heap + ArrayBuffer exceeded `limits.memoryMb`.
    MemoryLimit,
    /// Configured host global/import handler threw or rejected.
    HostBridge(String),
    /// PrefixRun attempted to bind a global not declared by Precompile.
    UndeclaredBinding(String),
    /// A function value was passed as a bridge argument.
    FunctionArgumentNotSupported,
    /// Unexpected internal runtime failure.
    Internal(String),
}

// ── Entry point ──────────────────────────────────────────────────────────────

/// Execute a sandboxed run and return the full output.
///
/// `globals` is the list of declared host-global names. Each name becomes a
/// bridge stub; when called from sandbox JS a `BridgeCall` frame goes out on
/// `stream_fd` and execution blocks until the matching `BridgeResponse`
/// arrives. Pass `None` when no globals are configured.
pub fn execute(
    code: &str,
    filename: Option<&str>,
    limits: Limits,
    globals: &[String],
    stream_fd: Option<RawFd>,
) -> Result<Output, FailureOutput> {
    init_platform();
    run_module(code, filename.unwrap_or("<iso4>"), None, limits, globals, stream_fd)
}

/// Execute a postfix against a pre-compiled prefix snapshot.
///
/// `globals` names are re-installed as fresh bridge stubs bound to `stream_fd`
/// for this run. Bridge stubs are never part of the snapshot - they are always
/// installed from scratch at run time.
pub fn execute_with_prefix(
    snapshot_bytes: &[u8],
    code: &str,
    filename: Option<&str>,
    limits: Limits,
    globals: &[String],
    stream_fd: Option<RawFd>,
) -> Result<Output, FailureOutput> {
    init_platform();
    run_module(code, filename.unwrap_or("<iso4>"), Some(snapshot_bytes), limits, globals, stream_fd)
}

/// Compile prefix code into a V8 startup snapshot blob.
///
/// Returns the raw snapshot bytes on success. The bytes can be stored and
/// passed to `execute_with_prefix` for many subsequent postfix runs.
///
/// Note: `console` is **not** available inside prefix code - native callbacks
/// cannot be snapshotted without `ExternalReferences` (a Phase 2 concern).
/// Prefix code that calls `console.*` will receive a `TypeError`.
///
/// No execution-time limits are applied: prefix code is host-authored and
/// trusted. If execution limits are needed for snapshot creation in future,
/// add an optional `limits: Limits` parameter here.
pub fn precompile(
    code: &str,
    filename: Option<&str>,
) -> Result<Vec<u8>, FailureOutput> {
    init_platform();
    precompile_module(code, filename.unwrap_or("<prefix>"))
}

/// Core execution without bridge - used by tests.
fn run_code(code: &str, filename: &str, limits: Limits) -> Result<Output, FailureOutput> {
    init_platform();
    run_module(code, filename, None, limits, &[], None)
}

/// ESM path: compile source as a module, instantiate it, evaluate it, then
/// inspect the module namespace object for `default` and named exports.
///
/// When `snapshot` is `Some(bytes)`, the isolate is created from that V8
/// startup snapshot so the prefix context is restored before postfix code runs.
/// When `globals` is non-empty, bridge stubs are installed for each name.
/// `stream_fd` is the raw file-descriptor of the session socket; it is used
/// only during bridge calls and is never closed by this function.
fn run_module(
    code: &str,
    filename: &str,
    snapshot: Option<&[u8]>,
    limits: Limits,
    globals: &[String],
    stream_fd: Option<RawFd>,
) -> Result<Output, FailureOutput> {
    let start = std::time::Instant::now();
    let mut logs = LogBuffers::default();

    let mut isolate = match snapshot {
        None => v8::Isolate::new(Default::default()),
        Some(bytes) => {
            let params = v8::Isolate::create_params().snapshot_blob(bytes.to_vec());
            v8::Isolate::new(params)
        }
    };
    isolate.set_microtasks_policy(v8::MicrotasksPolicy::Auto);

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
    let reason         = Arc::new(OnceLock::<TerminationReason>::new());
    let handle         = isolate.thread_safe_handle();
    let cancel_handle  = handle.clone(); // for cancel_terminate_execution on success
    let cpu_budget     = Arc::new(CpuBudget::new());
    let cancel_wall    = start_wall_guard(handle.clone(), Arc::clone(&reason), limits.wall_time_ms);
    let cancel_cpu     = start_cpu_guard(handle, Arc::clone(&reason), Arc::clone(&cpu_budget), limits.cpu_time_ms);
    // cpu_budget.enter() is called immediately before module.evaluate() so
    // compilation and scope setup time is not charged against the CPU budget.
    let _guard_canceller = GuardCanceller {
        cancel_wall: &cancel_wall,
        cancel_cpu:  &cancel_cpu,
        budget:      &cpu_budget,
    };

    let scope = &mut v8::HandleScope::new(&mut isolate);
    let context = v8::Context::new(scope, Default::default());
    let scope = &mut v8::ContextScope::new(scope, context);
    install_console(scope, &mut logs as *mut LogBuffers)
        .map_err(|error| failure(error, &logs, start))?;

    // ── Bridge globals setup ─────────────────────────────────────────────────
    //
    // Shared state for all bridge callbacks in this run. All three are Arcs-
    // no raw pointers to Rust objects:
    //   • cpu_budget   - already an Arc; cloned into each callback data block
    //   • call_id      - Arc<AtomicU32>, shared across all global stubs
    //   • bridge_error - Arc<OnceLock<RunError>>, first error wins
    //
    // The socket is carried as a raw file descriptor (RawFd = i32). The
    // callback reconstructs a ManuallyDrop<UnixStream> from it for the
    // duration of one BridgeCall/BridgeResponse exchange and never closes it.
    let call_id = Arc::new(AtomicU32::new(0));
    let bridge_error: Arc<OnceLock<RunError>> = Arc::new(OnceLock::new());

    // Box-per-stub allocations; kept alive until after evaluate() returns.
    let mut callback_data_boxes: Vec<Box<GlobalCallbackData>> = Vec::with_capacity(globals.len());
    if !globals.is_empty() {
        let fd = stream_fd.expect(
            "install_bridge_globals called with non-empty globals but no stream_fd"
        );
        install_bridge_globals(
            scope,
            globals,
            fd,
            Arc::clone(&cpu_budget),
            Arc::clone(&call_id),
            Arc::clone(&bridge_error),
            start,
            limits.wall_time_ms,
            cancel_handle.clone(), // thread-safe handle; cancel_handle is a clone
            Arc::clone(&reason),
            &mut callback_data_boxes,
        )
        .map_err(|e| failure(e, &logs, start))?;
    }

    let scope = &mut v8::TryCatch::new(scope);

    let source_string = v8::String::new(scope, code).ok_or_else(|| {
        failure(
            RunError::Internal("failed to intern module source".to_string()),
            &logs,
            start,
        )
    })?;
    let filename = v8::String::new(scope, filename).ok_or_else(|| {
        failure(
            RunError::Internal("failed to intern filename".to_string()),
            &logs,
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
                RunError::CompileError(exception_message(scope)),
                &logs,
                start,
            ))
        }
    };

    if module
        .instantiate_module(scope, no_import_resolver)
        .is_none()
    {
        return Err(failure(
            RunError::ModuleNotFound(exception_message(scope)),
            &logs,
            start,
        ));
    }

    cpu_budget.enter(); // start measuring active CPU time (compile + scope setup excluded)
    let evaluation = match module.evaluate(scope) {
        Some(value) => {
            // Run completed - cancel guards before inspecting exports.
            cancel_guards(&cancel_wall, &cancel_cpu, &cpu_budget);
            // Clear any sticky termination flag a late-firing guard may have
            // set after evaluate() returned, so export extraction succeeds.
            cancel_handle.cancel_terminate_execution();
            value
        }
        None => {
            cancel_guards(&cancel_wall, &cancel_cpu, &cpu_budget);
            // Check bridge error first - it takes priority over generic runtime errors.
            if let Some(err) = bridge_error.get() {
                let owned = match err {
                    RunError::HostBridge(m)              => RunError::HostBridge(m.clone()),
                    RunError::FunctionArgumentNotSupported => RunError::FunctionArgumentNotSupported,
                    other => RunError::Internal(format!("unexpected bridge error: {other:?}")),
                };
                return Err(failure(owned, &logs, start));
            }
            let error = match reason.get().copied() {
                Some(TerminationReason::Wall) => RunError::WallTimeout,
                Some(TerminationReason::Cpu)  => RunError::CpuTimeout,
                None => RunError::RuntimeError {
                    message: exception_message(scope),
                    stack:   exception_stack(scope),
                },
            };
            return Err(failure(error, &logs, start));
        }
    };

    if evaluation.is_promise() {
        let promise = v8::Local::<v8::Promise>::try_from(evaluation).map_err(|_| {
            failure(
                RunError::Internal("failed to inspect module evaluation promise".to_string()),
                &logs,
                start,
            )
        })?;
        match promise.state() {
            v8::PromiseState::Fulfilled => {}
            v8::PromiseState::Rejected => {
                // Read the rejection value while the scope is still valid.
                // Guards were already cancelled in the Some(value) arm above.
                let rejection = promise.result(scope);
                // Bridge error takes priority over the JS-level rejection
                // (which is just the sentinel we threw to unwind the stack).
                if let Some(err) = bridge_error.get() {
                    let owned = match err {
                        RunError::HostBridge(m)               => RunError::HostBridge(m.clone()),
                        RunError::FunctionArgumentNotSupported => RunError::FunctionArgumentNotSupported,
                        other => RunError::Internal(format!("unexpected bridge error: {other:?}")),
                    };
                    return Err(failure(owned, &logs, start));
                }
                return Err(failure(
                    runtime_error_from_value(scope, rejection),
                    &logs,
                    start,
                ));
            }
            v8::PromiseState::Pending => {
                // Guards were already cancelled in the Some(value) arm above.
                // A pending promise after evaluate() normally means user
                // code exported an un-awaited Promise.  But it can also
                // happen when terminate_execution() fires between the bridge
                // callback's return and the microtask that runs the await
                // continuation - V8 exits before the module fully settles.
                // Check the termination reason first so we surface the
                // correct error code instead of ExportNotSerializable.
                if let Some(err) = bridge_error.get() {
                    let owned = match err {
                        RunError::HostBridge(m)               => RunError::HostBridge(m.clone()),
                        RunError::FunctionArgumentNotSupported => RunError::FunctionArgumentNotSupported,
                        other => RunError::Internal(format!("unexpected bridge error: {other:?}")),
                    };
                    return Err(failure(owned, &logs, start));
                }
                let error = match reason.get().copied() {
                    Some(TerminationReason::Wall) => RunError::WallTimeout,
                    Some(TerminationReason::Cpu)  => RunError::CpuTimeout,
                    None => RunError::ExportNotSerializable(
                        "module evaluation promise is still pending".to_string(),
                    ),
                };
                return Err(failure(error, &logs, start));
            }
        }
    }

    let namespace = module
        .get_module_namespace()
        .to_object(scope)
        .ok_or_else(|| {
            failure(
                RunError::Internal("module namespace is not an object".to_string()),
                &logs,
                start,
            )
        })?;

    let names = namespace
        .get_own_property_names(scope, v8::GetPropertyNamesArgs::default())
        .ok_or_else(|| {
            failure(
                RunError::Internal("failed to read module export names".to_string()),
                &logs,
                start,
            )
        })?;

    let mut fields: Vec<(String, WireValue)> = Vec::new();

    for i in 0..names.length() {
        let name_value = names.get_index(scope, i).ok_or_else(|| {
            failure(
                RunError::Internal("failed to read export name".to_string()),
                &logs,
                start,
            )
        })?;
        let name = name_value
            .to_string(scope)
            .map(|s| s.to_rust_string_lossy(scope))
            .ok_or_else(|| {
                failure(
                    RunError::Internal("failed to stringify export name".to_string()),
                    &logs,
                    start,
                )
            })?;

        let value = namespace.get(scope, name_value).ok_or_else(|| {
            failure(
                RunError::Internal(format!("failed to read export {name}")),
                &logs,
                start,
            )
        })?;

        let wire =
            export_to_wire(scope, &name, value).map_err(|error| failure(error, &logs, start))?;
        fields.push((name, wire));
    }

    Ok(Output {
        exports: WireValue::Object(fields),
        stdout: logs.stdout.clone(),
        stderr: logs.stderr.clone(),
        duration_ms: start.elapsed().as_millis() as u64,
    })
}

/// Compile and snapshot prefix code into a raw V8 startup blob.
///
/// Uses `Isolate::snapshot_creator` instead of `Isolate::new`. The scope
/// block must drop before `create_blob` is called, so compilation runs inside
/// an immediately-invoked closure that borrows `&mut isolate`.
fn precompile_module(code: &str, filename: &str) -> Result<Vec<u8>, FailureOutput> {
    let start = std::time::Instant::now();

    let mut isolate = v8::Isolate::snapshot_creator(None, None);
    isolate.set_microtasks_policy(v8::MicrotasksPolicy::Auto);

    // All V8 scopes must be dropped before create_blob is called.
    // The IIFE ensures the &mut isolate borrow ends when the closure returns.
    let compile_result: Result<(), FailureOutput> = (|| {
        let mut logs = LogBuffers::default();

        let scope = &mut v8::HandleScope::new(&mut isolate);
        let context = v8::Context::new(scope, Default::default());
        let scope = &mut v8::ContextScope::new(scope, context);

        // Mark this context as the snapshot default BEFORE creating TryCatch.
        // Must be called while the context Local is still alive.
        scope.set_default_context(context);

        let scope = &mut v8::TryCatch::new(scope);

        let source_string = v8::String::new(scope, code).ok_or_else(|| {
            failure(
                RunError::Internal("failed to intern module source".to_string()),
                &logs,
                start,
            )
        })?;
        let filename_str = v8::String::new(scope, filename).ok_or_else(|| {
            failure(
                RunError::Internal("failed to intern filename".to_string()),
                &logs,
                start,
            )
        })?;
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
                return Err(failure(
                    RunError::CompileError(exception_message(scope)),
                    &logs,
                    start,
                ))
            }
        };

        if module
            .instantiate_module(scope, no_import_resolver)
            .is_none()
        {
            return Err(failure(
                RunError::ModuleNotFound(exception_message(scope)),
                &logs,
                start,
            ));
        }

        let evaluation = match module.evaluate(scope) {
            Some(v) => v,
            None => {
                return Err(failure(
                    RunError::RuntimeError {
                        message: exception_message(scope),
                        stack: exception_stack(scope),
                    },
                    &logs,
                    start,
                ))
            }
        };

        if evaluation.is_promise() {
            let promise =
                v8::Local::<v8::Promise>::try_from(evaluation).map_err(|_| {
                    failure(
                        RunError::Internal(
                            "failed to inspect module evaluation promise".to_string(),
                        ),
                        &logs,
                        start,
                    )
                })?;
            match promise.state() {
                v8::PromiseState::Fulfilled => {}
                v8::PromiseState::Rejected => {
                    let rejection = promise.result(scope);
                    return Err(failure(
                        runtime_error_from_value(scope, rejection),
                        &logs,
                        start,
                    ));
                }
                v8::PromiseState::Pending => {
                    return Err(failure(
                        RunError::ExportNotSerializable(
                            "module evaluation promise is still pending".to_string(),
                        ),
                        &logs,
                        start,
                    ));
                }
            }
        }

        Ok(())
    })();

    // V8 requires create_blob to be called before dropping a snapshot-creator
    // isolate, even when compilation failed. We call it unconditionally and
    // only use the blob on the success path.
    let snapshot_opt = isolate.create_blob(v8::FunctionCodeHandling::Keep);

    compile_result?;

    snapshot_opt
        .map(|s| s.to_vec())
        .ok_or_else(|| FailureOutput {
            error: RunError::Internal(
                "V8 snapshot creation returned an empty blob".to_string(),
            ),
            stdout: Vec::new(),
            stderr: Vec::new(),
            duration_ms: start.elapsed().as_millis() as u64,
        })
}

fn failure(error: RunError, logs: &LogBuffers, start: std::time::Instant) -> FailureOutput {
    FailureOutput {
        error,
        stdout: logs.stdout.clone(),
        stderr: logs.stderr.clone(),
        duration_ms: start.elapsed().as_millis() as u64,
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

// ── Helpers ──────────────────────────────────────────────────────────────────

// ── Limit guard helpers ────────────────────────────────────────────────────

/// Spawn a wall-clock guard thread. Returns a cancel sender.
/// Sending anything on it (or dropping it) cancels the guard.
/// If `wall_time_ms == 0`, no thread is spawned but a sender is still returned.
fn start_wall_guard(
    handle: v8::IsolateHandle,
    reason: Arc<OnceLock<TerminationReason>>,
    wall_time_ms: u32,
) -> crossbeam_channel::Sender<()> {
    let (tx, rx) = crossbeam_channel::bounded::<()>(1);
    if wall_time_ms > 0 {
        std::thread::spawn(move || {
            let timeout = Duration::from_millis(wall_time_ms as u64);
            if let Err(crossbeam_channel::RecvTimeoutError::Timeout) = rx.recv_timeout(timeout) {
                reason.set(TerminationReason::Wall).ok(); // first writer wins
                handle.terminate_execution();
            }
            // Err(Disconnected) means cancel_guards() fired first - do nothing.
        });
    }
    tx
}

/// Spawn a CPU-budget guard thread. Returns a cancel sender.
/// Polls `budget.elapsed_ms()` every 10 ms; fires when it exceeds `cpu_time_ms`.
/// Phase 4 hook: call `budget.leave()` before bridge calls and `budget.enter()`
/// after to exclude host wait time from the measurement.
fn start_cpu_guard(
    handle: v8::IsolateHandle,
    reason: Arc<OnceLock<TerminationReason>>,
    budget: Arc<CpuBudget>,
    cpu_time_ms: u32,
) -> crossbeam_channel::Sender<()> {
    let (tx, rx) = crossbeam_channel::bounded::<()>(1);
    if cpu_time_ms > 0 {
        std::thread::spawn(move || loop {
            match rx.recv_timeout(Duration::from_millis(10)) {
                Ok(_) | Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => {
                    if budget.elapsed_ms() >= cpu_time_ms as u64 {
                        reason.set(TerminationReason::Cpu).ok(); // first writer wins
                        handle.terminate_execution();
                        break;
                    }
                }
            }
        });
    }
    tx
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
    cancel_cpu:  &'a crossbeam_channel::Sender<()>,
    budget:      &'a CpuBudget,
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
//   1. Serialises the JS arguments to WireValues (rejects function args).
//   2. Calls cpu_budget.leave() to pause the CPU budget during host wait.
//   3. Writes a BridgeCall frame on the session socket.
//   4. Blocks reading a BridgeResponse frame (the V8 thread is already blocked
//      in the host callback, so no V8 activity can happen during this wait).
//   5. Calls cpu_budget.enter() to resume counting.
//   6. On success: deserialises the WireValue result back to a V8 value.
//   7. On error: stores RunError::HostBridge in the shared OnceLock,
//      throws a JS exception to unwind the module, and lets run_module
//      surface the correct error code.
//
// fetch is NOT special. It gets the same callback as every other global.
// The host handler decides what the arguments mean and what to return.

/// Per-stub heap allocation passed as External data to `bridge_global_callback`.
/// One instance per declared global name, allocated as `Box<GlobalCallbackData>`
/// and kept alive for the duration of `run_module`.
struct GlobalCallbackData {
    /// Raw file descriptor for the session socket. Bridge callbacks reconstruct
    /// a `ManuallyDrop<UnixStream>` from this for each round-trip. The fd is
    /// never closed here - it remains owned by the session thread.
    stream_fd: RawFd,
    /// Shared CPU budget. Paused with `leave()` before bridge I/O, resumed
    /// with `enter()` after - so host-wait time doesn't count against the
    /// sandbox CPU budget.
    cpu_budget: Arc<CpuBudget>,
    /// Monotonic bridge call-ID counter, shared across all stubs in one run.
    call_id: Arc<AtomicU32>,
    /// First bridge error wins. Written once, read after evaluate() returns.
    bridge_error: Arc<OnceLock<RunError>>,
    /// The global name this stub is registered for (e.g. "fetch", "myTool").
    name: String,
    /// When the run started. Combined with `wall_time_ms` to compute the
    /// remaining wall budget before each blocking bridge read.
    wall_start: Instant,
    /// Wall-clock budget for the whole run in milliseconds. When non-zero, a
    /// socket read timeout equal to the remaining wall time is set before
    /// every blocking bridge read. If the timeout fires the callback calls
    /// `terminate_execution()` directly so termination is not racy.
    /// Zero means no wall limit - blocking reads run without a timeout.
    wall_time_ms: u32,
    /// Thread-safe handle to the running isolate. Used to call
    /// `terminate_execution()` when a wall-timeout fires mid-bridge-wait so
    /// V8 is guaranteed to terminate even if the wall guard's own call races.
    isolate_handle: v8::IsolateHandle,
    /// Shared termination-reason slot. The callback writes `Wall` here before
    /// calling `terminate_execution()` so that `run_module` maps the result
    /// to `ERR_WALL_TIMEOUT` rather than a generic RuntimeError.
    termination_reason: Arc<OnceLock<TerminationReason>>,
}

/// A single generic V8 FunctionCallback used for every host-declared global.
///
/// Serialises arguments, does a synchronous BridgeCall/BridgeResponse round
/// trip on the session socket, then injects the result back into V8.
fn bridge_global_callback(
    scope: &mut v8::HandleScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    // Recover the per-stub data from the External attached at registration time.
    // SAFETY: the pointer is valid for the lifetime of run_module; the callback
    // is only called during module.evaluate() which is nested inside run_module.
    let data_ptr = args.data();
    let data = match v8::Local::<v8::External>::try_from(data_ptr) {
        Ok(ext) => ext.value().cast::<GlobalCallbackData>(),
        Err(_) => {
            throw_v8_error(scope, "[iso4] bridge: missing external data");
            return;
        }
    };
    // SAFETY: `data_ptr` was placed in the External by `install_bridge_globals`
    // via `Box::as_ref() as *const _ as *mut c_void`. The owning Box lives in
    // `callback_data_boxes` inside `run_module`, which is kept alive until after
    // `module.evaluate()` returns. No callback fires after that.
    let data = unsafe { &*data };

    // ── Serialise arguments ────────────────────────────────────────────────
    let mut wire_args: Vec<WireValue> = Vec::with_capacity(args.length() as usize);
    for i in 0..args.length() {
        let arg = args.get(i);
        if arg.is_function() {
            data.bridge_error.set(RunError::FunctionArgumentNotSupported).ok();
            throw_v8_error(scope, "[iso4] bridge: function arguments are not supported");
            return;
        }
        match arg_to_wire(scope, arg) {
            Ok(wv) => wire_args.push(wv),
            Err(e) => {
                data.bridge_error.set(e).ok();
                throw_v8_error(scope, "[iso4] bridge: failed to serialise argument");
                return;
            }
        }
    }

    // ── CPU budget: stop counting during host wait ───────────────────────────
    data.cpu_budget.leave();

    // ── Assign a unique call ID ────────────────────────────────────────────
    let call_id = data.call_id.fetch_add(1, Ordering::Relaxed);

    // ── Reconstruct the socket for this bridge round-trip ────────────────────
    // SAFETY: stream_fd is the file descriptor of the live session socket.
    // We wrap it in ManuallyDrop so it is never closed by the local variable.
    // V8 is blocked in this callback - no other reader/writer touches the fd.
    let mut stream = ManuallyDrop::new(unsafe { UnixStream::from_raw_fd(data.stream_fd) });

    // ── Send BridgeCall frame ──────────────────────────────────────────────
    let bridge_call_payload =
        wire::encode_bridge_call_payload(call_id, 0, None, &data.name, &wire_args);

    if let Err(e) = ipc::write_rust_to_ts_frame(
        &mut *stream,
        ipc::RustToTsMessageType::BridgeCall,
        &bridge_call_payload,
    ) {
        data.bridge_error
            .set(RunError::Internal(format!("bridge write failed: {e}")))
            .ok();
        data.cpu_budget.enter();
        throw_v8_error(scope, "[iso4] bridge: send failed");
        return;
    }

    // ── Block waiting for BridgeResponse ──────────────────────────────────
    // The session socket has no other reader while V8 is blocked here (v1:
    // bridge calls are sequential within a run).
    //
    // When a wall limit is configured, set the socket read timeout to the
    // remaining wall budget so a stalled TS handler is killed by this callback
    // rather than the wall guard thread (avoids a race). Without a wall limit
    // we block indefinitely — the wall guard is the only termination signal.
    // If the TS host crashes without a wall limit that is a larger ops problem;
    // the process-level wall timeout from the host side handles it.
    if data.wall_time_ms > 0 {
        let elapsed   = data.wall_start.elapsed();
        let budget    = Duration::from_millis(data.wall_time_ms as u64);
        let remaining = budget.saturating_sub(elapsed).max(Duration::from_millis(1));
        stream.set_read_timeout(Some(remaining)).ok();
    }

    let response_frame = ipc::read_ts_to_rust_frame(&mut *stream);

    // Restore blocking mode so subsequent bridge calls on the same fd are
    // not affected by the timeout we may have set above.
    if data.wall_time_ms > 0 {
        stream.set_read_timeout(None).ok();
    }

    // ── CPU budget: resume counting ──────────────────────────────────────────
    data.cpu_budget.enter();

    let frame = match response_frame {
        Ok(f) => f,
        Err(e) => {
            if e.kind() == std::io::ErrorKind::WouldBlock
                || e.kind() == std::io::ErrorKind::TimedOut
            {
                // The wall budget was exhausted while we were waiting for the
                // host handler. Explicitly set the termination reason and
                // terminate V8 from this callback - we cannot rely on the
                // wall guard thread's own call winning the race.
                //
                // The handler promise on the TypeScript side is now orphaned.
                // Node.js cannot forcefully kill it; any in-flight I/O or
                // timers inside the handler will run to natural completion.
                // Handlers that accept an AbortSignal can self-cancel.
                // See DESIGN.md §15.4.
                data.termination_reason.set(TerminationReason::Wall).ok();
                data.isolate_handle.terminate_execution();
                return; // V8 will terminate at the next safe point
            }
            data.bridge_error
                .set(RunError::Internal(format!("bridge read failed: {e}")))
                .ok();
            throw_v8_error(scope, "[iso4] bridge: read failed");
            return;
        }
    };

    if frame.message_type != ipc::TsToRustMessageType::BridgeResponse {
        data.bridge_error
            .set(RunError::Internal(format!(
                "expected BridgeResponse, got {:?}", frame.message_type
            )))
            .ok();
        throw_v8_error(scope, "[iso4] bridge: protocol error");
        return;
    }

    // ── Decode BridgeResponse ──────────────────────────────────────────────
    let response = match wire::parse_bridge_response_payload(&frame.payload) {
        Ok(r) => r,
        Err(e) => {
            data.bridge_error
                .set(RunError::Internal(format!("bridge response decode: {e}")))
                .ok();
            throw_v8_error(scope, "[iso4] bridge: response decode failed");
            return;
        }
    };

    match response {
        Ok(wire_value) => {
            match wire_to_v8_value(scope, &wire_value) {
                Some(v8_val) => rv.set(v8_val),
                None => throw_v8_error(scope, "[iso4] bridge: failed to convert return value"),
            }
        }
        Err(error_message) => {
            data.bridge_error
                .set(RunError::HostBridge(error_message.clone()))
                .ok();
            throw_v8_error(scope, &format!("[iso4] host bridge error: {error_message}"));
        }
    }
}

/// Install a bridge stub for each declared global name.
///
/// Each stub is an identical `bridge_global_callback` function with per-name
/// `GlobalCallbackData` attached as External data. The boxes are pushed into
/// `out_boxes` so their heap allocations outlive the V8 evaluation.
fn install_bridge_globals(
    scope: &mut v8::HandleScope,
    globals: &[String],
    stream_fd: RawFd,
    cpu_budget: Arc<CpuBudget>,
    call_id: Arc<AtomicU32>,
    bridge_error: Arc<OnceLock<RunError>>,
    wall_start: Instant,
    wall_time_ms: u32,
    isolate_handle: v8::IsolateHandle,
    termination_reason: Arc<OnceLock<TerminationReason>>,
    out_boxes: &mut Vec<Box<GlobalCallbackData>>,
) -> Result<(), RunError> {
    let global_obj = scope.get_current_context().global(scope);
    for name in globals {
        let data = Box::new(GlobalCallbackData {
            stream_fd,
            cpu_budget: Arc::clone(&cpu_budget),
            call_id: Arc::clone(&call_id),
            bridge_error: Arc::clone(&bridge_error),
            name: name.clone(),
            wall_start,
            wall_time_ms,
            isolate_handle: isolate_handle.clone(),
            termination_reason: Arc::clone(&termination_reason),
        });
        // Pass a raw pointer to the Box's heap allocation as External data.
        // The Box is stored in out_boxes and outlives all V8 callbacks.
        let data_ptr = data.as_ref() as *const GlobalCallbackData as *mut c_void;
        out_boxes.push(data);

        let external = v8::External::new(scope, data_ptr);
        let function = v8::Function::builder(bridge_global_callback)
            .data(external.into())
            .build(scope)
            .ok_or_else(|| RunError::Internal(
                format!("failed to build bridge stub for '{name}'"),
            ))?;

        let key = v8::String::new(scope, name)
            .ok_or_else(|| RunError::Internal(
                format!("failed to intern global name '{name}'"),
            ))?;
        global_obj
            .set(scope, key.into(), function.into())
            .ok_or_else(|| RunError::Internal(
                format!("failed to install global '{name}'"),
            ))?;
    }
    Ok(())
}

/// Convert a V8 value back to a `WireValue` for injection into the sandbox.
///
/// Used to materialise the host's bridge response back into the JS context.
/// Returns `None` only when V8 string allocation fails (essentially never).
fn wire_to_v8_value<'s>(
    scope: &mut v8::HandleScope<'s>,
    value: &WireValue,
) -> Option<v8::Local<'s, v8::Value>> {
    match value {
        WireValue::Undefined => Some(v8::undefined(scope).into()),
        WireValue::Null      => Some(v8::null(scope).into()),
        WireValue::Bool(b)   => Some(v8::Boolean::new(scope, *b).into()),
        WireValue::Number(n) => Some(v8::Number::new(scope, *n).into()),
        WireValue::String(s) => v8::String::new(scope, s).map(|s| s.into()),
        WireValue::BigInt(s) => {
            // Supports values in [i64::MIN, u64::MAX].
            // Values outside this range return None (bridge injection fails).
            // Full arbitrary-precision support via v8::BigInt::new_from_words
            // is deferred - see notes/deferred-fixes.md.
            if let Ok(n) = s.parse::<i64>() {
                return Some(v8::BigInt::new_from_i64(scope, n).into());
            }
            if let Ok(n) = s.parse::<u64>() {
                return Some(v8::BigInt::new_from_u64(scope, n).into());
            }
            None // out-of-range: signal injection failure to caller
        }
        WireValue::Bytes(b) => {
            let len = b.len();
            let store =
                v8::ArrayBuffer::new_backing_store_from_vec(b.to_vec())
                    .make_shared();
            let ab = v8::ArrayBuffer::with_backing_store(scope, &store);
            v8::Uint8Array::new(scope, ab, 0, len).map(|a| a.into())
        }
        WireValue::Array(items) => {
            let array = v8::Array::new(scope, items.len() as i32);
            for (i, item) in items.iter().enumerate() {
                let v = wire_to_v8_value(scope, item)?;
                if array.set_index(scope, i as u32, v).is_none() {
                    return None;
                }
            }
            Some(array.into())
        }
        WireValue::Object(fields) => {
            let obj = v8::Object::new(scope);
            for (key, val) in fields {
                // Drop "__proto__" — silently elided in both directions.
                // `serialize_object_fields` already drops it sandbox→host;
                // we mirror that here for host→sandbox so the behaviour is
                // symmetric.  Even though `create_data_property`
                // ([[DefineOwnProperty]]) would store it as a plain own data
                // property without touching the prototype chain, the protocol
                // policy is: "__proto__" keys never cross either boundary.
                if key == "__proto__" {
                    continue;
                }
                let k = v8::String::new(scope, key)?;
                let v = wire_to_v8_value(scope, val)?;
                if obj.create_data_property(scope, k.into(), v).is_none() {
                    return None;
                }
            }
            Some(obj.into())
        }
    }
}

/// Throw a plain Error into V8 with the given message.
/// Splits the string-creation, exception-creation, and throw into separate
/// borrows so the compiler doesn't see `scope` used twice simultaneously.
fn throw_v8_error(scope: &mut v8::HandleScope, message: &str) {
    if let Some(s) = v8::String::new(scope, message) {
        let exc = v8::Exception::error(scope, s);
        scope.throw_exception(exc);
    }
}

/// Serialise a V8 value for use as a bridge call argument.
///
/// Like `value_to_wire` but uses `RunError::FunctionArgumentNotSupported` for
/// function values instead of `ExportNotSerializable`.
/// Wraps in a TryCatch internally so it can reuse `value_to_wire`.
fn arg_to_wire(
    scope: &mut v8::HandleScope,
    value: v8::Local<v8::Value>,
) -> Result<WireValue, RunError> {
    if value.is_function() {
        return Err(RunError::FunctionArgumentNotSupported);
    }
    let scope = &mut v8::TryCatch::new(scope);
    let mut visiting = Vec::new();
    value_to_wire(scope, value, &mut visiting)
}

fn install_console(scope: &mut v8::HandleScope, buffers: *mut LogBuffers) -> Result<(), RunError> {
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
    scope: &mut v8::HandleScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    append_console_line(scope, args, true);
    rv.set_undefined();
}

fn console_stderr_callback(
    scope: &mut v8::HandleScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    append_console_line(scope, args, false);
    rv.set_undefined();
}

fn append_console_line(
    scope: &mut v8::HandleScope,
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
        buffers.stdout.push(line);
    } else {
        buffers.stderr.push(line);
    }
}

/// Convert a top-level module export value to a `WireValue`.
///
/// Adds the export `name` to error messages for functions/promises.
/// Initialises the identity-based visiting set used by `value_to_wire`.
fn export_to_wire(
    scope: &mut v8::TryCatch<v8::HandleScope>,
    name: &str,
    value: v8::Local<v8::Value>,
) -> Result<WireValue, RunError> {
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
    let mut visiting: Vec<v8::Global<v8::Value>> = Vec::new();
    value_to_wire(scope, value, &mut visiting)
}

/// Check whether `value` is already on the current recursion path (`visiting`).
///
/// Uses `strict_equals` - V8 reference equality - so two distinct JS objects
/// that happen to have the same shape are never confused.
///
/// Returns `Ok(())` and pushes `value` if no cycle is found.
/// Returns `Err` without pushing if a cycle is detected; the caller must NOT
/// call `visiting.pop()` in the error branch.
fn check_and_push(
    scope: &mut v8::TryCatch<v8::HandleScope>,
    value: v8::Local<v8::Value>,
    visiting: &mut Vec<v8::Global<v8::Value>>,
) -> Result<(), RunError> {
    for visited_global in visiting.iter() {
        let visited_local = v8::Local::new(scope.as_mut(), visited_global);
        if value.strict_equals(visited_local) {
            return Err(RunError::ExportNotSerializable(
                "cyclic or self-referential structure detected in export value"
                    .to_string(),
            ));
        }
    }
    visiting.push(v8::Global::new(scope.as_mut(), value));
    Ok(())
}

/// Serialise the items of a V8 array that has already been pushed onto
/// `visiting` by the caller.
fn serialize_array_items(
    scope: &mut v8::TryCatch<v8::HandleScope>,
    array: v8::Local<v8::Array>,
    visiting: &mut Vec<v8::Global<v8::Value>>,
) -> Result<Vec<WireValue>, RunError> {
    let len = array.length();
    let mut items = Vec::with_capacity(len as usize);
    for i in 0..len {
        let item = array
            .get_index(scope, i)
            .ok_or_else(|| RunError::Internal(format!("failed to read array index {i}")))?;
        items.push(value_to_wire(scope, item, visiting)?);
    }
    Ok(items)
}

/// Serialise the own enumerable string-keyed properties of a V8 object that
/// has already been pushed onto `visiting` by the caller.
fn serialize_object_fields(
    scope: &mut v8::TryCatch<v8::HandleScope>,
    object: v8::Local<v8::Object>,
    visiting: &mut Vec<v8::Global<v8::Value>>,
) -> Result<Vec<(String, WireValue)>, RunError> {
    let names = object
        .get_own_property_names(scope, v8::GetPropertyNamesArgs::default())
        .ok_or_else(|| RunError::Internal("failed to get own property names".to_string()))?;
    let mut fields = Vec::with_capacity(names.length() as usize);
    for i in 0..names.length() {
        let name_value = names
            .get_index(scope, i)
            .ok_or_else(|| RunError::Internal("failed to read property name".to_string()))?;
        let name = name_value
            .to_string(scope)
            .map(|s| s.to_rust_string_lossy(scope))
            .ok_or_else(|| {
                RunError::Internal("failed to stringify property name".to_string())
            })?;
        // Drop "__proto__" before it crosses the bridge.  Protocol policy:
        // "__proto__" keys are silently elided in both directions — here
        // (sandbox→host) and in `wire_to_v8_value` (host→sandbox).
        // Defence-in-depth: the TS decoder also uses Object.create(null)
        // so even if this guard were absent the TS side would store
        // "__proto__" as a plain data property without touching any
        // prototype chain.  The guard remains for belt-and-suspenders
        // and for symmetry with the host→sandbox drop.
        if name == "__proto__" {
            continue;
        }
        let val = object
            .get(scope, name_value)
            .ok_or_else(|| RunError::Internal(format!("failed to read property {name}")))?;
        fields.push((name, value_to_wire(scope, val, visiting)?));
    }
    Ok(fields)
}

/// Recursively convert a V8 value to a `WireValue`.
///
/// `visiting` is the set of reference-type values (objects/arrays) currently
/// on the recursion path. It acts as a call-stack to detect cycles:
/// - pushed when entering an object or array
/// - popped when leaving (whether success or error)
/// - compared with `strict_equals` (V8 reference equality) at each entry
///
/// Shared but non-cyclic references (the same object appearing in two
/// different fields) are serialised correctly: the object is popped after
/// the first field, so it is not in the set when the second field is visited.
fn value_to_wire(
    scope: &mut v8::TryCatch<v8::HandleScope>,
    value: v8::Local<v8::Value>,
    visiting: &mut Vec<v8::Global<v8::Value>>,
) -> Result<WireValue, RunError> {
    // ── Primitives (no cycle risk) ────────────────────────────────────────
    if value.is_undefined() {
        return Ok(WireValue::Undefined);
    }
    if value.is_null() {
        return Ok(WireValue::Null);
    }
    if value.is_boolean() {
        return Ok(WireValue::Bool(value.boolean_value(scope)));
    }
    if value.is_number() {
        return Ok(WireValue::Number(
            value.number_value(scope).unwrap_or(f64::NAN),
        ));
    }
    if value.is_string() {
        let s = value
            .to_string(scope)
            .map(|s| s.to_rust_string_lossy(scope))
            .ok_or_else(|| {
                RunError::Internal("failed to convert V8 string value".to_string())
            })?;
        return Ok(WireValue::String(s));
    }
    if value.is_big_int() {
        let bigint = v8::Local::<v8::BigInt>::try_from(value)
            .map_err(|_| RunError::Internal("failed to cast to BigInt".to_string()))?;
        let (i_val, fits_i64) = bigint.i64_value();
        if fits_i64 {
            return Ok(WireValue::BigInt(i_val.to_string()));
        }
        let (u_val, fits_u64) = bigint.u64_value();
        if fits_u64 {
            return Ok(WireValue::BigInt(u_val.to_string()));
        }
        return Err(RunError::ExportNotSerializable(
            "BigInt value is out of the supported i64/u64 range for wire encoding"
                .to_string(),
        ));
    }
    if value.is_symbol() {
        return Err(RunError::ExportNotSerializable(
            "Symbol values cannot be serialized".to_string(),
        ));
    }
    // ── Reference types - cycle detection applies ─────────────────────────
    if value.is_function() {
        return Err(RunError::ExportNotSerializable(
            "function values cannot be serialized".to_string(),
        ));
    }
    if value.is_promise() {
        return Err(RunError::ExportNotSerializable(
            "unresolved Promise values cannot be serialized".to_string(),
        ));
    }
    // Arrays must be checked before the generic object path (arrays are objects).
    if value.is_array() {
        check_and_push(scope, value, visiting)?;
        let result = match v8::Local::<v8::Array>::try_from(value) {
            Ok(arr) => serialize_array_items(scope, arr, visiting).map(WireValue::Array),
            Err(_) => Err(RunError::Internal("failed to cast to Array".to_string())),
        };
        visiting.pop();
        return result;
    }
    if value.is_object() {
        check_and_push(scope, value, visiting)?;
        let result = match value.to_object(scope) {
            Some(obj) => serialize_object_fields(scope, obj, visiting).map(WireValue::Object),
            None => Err(RunError::Internal("failed to cast to object".to_string())),
        };
        visiting.pop();
        return result;
    }

    Err(RunError::ExportNotSerializable(
        "unsupported or unknown value type".to_string(),
    ))
}

fn runtime_error_from_value(
    scope: &mut v8::TryCatch<v8::HandleScope>,
    value: v8::Local<v8::Value>,
) -> RunError {
    let message = error_message_from_value(scope, value)
        .or_else(|| {
            value
                .to_string(scope)
                .map(|s| s.to_rust_string_lossy(scope))
        })
        .unwrap_or_else(|| "JavaScript error".to_string());
    RunError::RuntimeError {
        message,
        stack: stack_from_value(scope, value),
    }
}

fn error_message_from_value(
    scope: &mut v8::TryCatch<v8::HandleScope>,
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
    scope: &mut v8::TryCatch<v8::HandleScope>,
    value: v8::Local<v8::Value>,
) -> Option<String> {
    let object = value.to_object(scope)?;
    let key = v8::String::new(scope, "stack")?;
    object
        .get(scope, key.into())?
        .to_string(scope)
        .map(|s| s.to_rust_string_lossy(scope))
}

fn exception_message(scope: &mut v8::TryCatch<v8::HandleScope>) -> String {
    scope
        .exception()
        .and_then(|e| e.to_string(scope))
        .map(|s| s.to_rust_string_lossy(scope))
        .unwrap_or_else(|| "(no exception message)".to_string())
}

fn exception_stack(scope: &mut v8::TryCatch<v8::HandleScope>) -> Option<String> {
    let exception = scope.exception()?;
    stack_from_value(scope, exception)
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
    use crate::wire::WireValue;

    /// Shorthand: run a code string and return the full Output or RunError.
    fn run(code: &str) -> Result<Output, RunError> {
        run_code(code, "<iso4>", Limits::default()).map_err(|failure| failure.error)
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

    // ── WireValue test helpers ────────────────────────────────────────────────
    //
    // These convert the new WireValue-based Output back to the human-readable
    // string representations that the original tests expected. This keeps the
    // test bodies unchanged (or nearly so) while the internal representation
    // moves from stringified exports to structured WireValue.

    /// Look up a field in the top-level exports Object by name.
    fn get_field(out: &Output, key: &str) -> Option<WireValue> {
        if let WireValue::Object(fields) = &out.exports {
            fields.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
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
        if let WireValue::Object(fields) = &out.exports {
            fields.iter().map(|(k, _)| k.clone()).collect()
        } else {
            vec![]
        }
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
            WireValue::BigInt(s) => s.clone(),
            WireValue::Bytes(_) => "[Uint8Array]".to_string(),
            WireValue::Array(items) => {
                let parts: Vec<String> =
                    items.iter().map(wire_to_display_str).collect();
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
        assert!(matches!(err, RunError::RuntimeError { .. }));
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
        assert!(matches!(err, RunError::RuntimeError { .. }));
    }

    #[test]
    fn thrown_error_message_is_preserved() {
        let err = run_err(r#"throw new Error("specific message")"#);
        if let RunError::RuntimeError { message, .. } = err {
            assert!(message.contains("specific message"));
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

        assert!(matches!(failure.error, RunError::RuntimeError { .. }));
        assert!(has_line(&failure.stdout, "before stdout"));
        assert!(has_line(&failure.stderr, "before stderr"));
    }

    #[test]
    fn thrown_string_is_runtime_error() {
        let err = run_err(r#"throw "raw string error""#);
        assert!(matches!(err, RunError::RuntimeError { .. }));
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
        if let RunError::RuntimeError { stack, .. } = err {
            assert!(stack.is_some(), "expected a stack trace");
        } else {
            panic!("expected RuntimeError");
        }
    }

    #[test]
    fn reference_error_is_runtime_error() {
        let err = run_err("export default undeclaredVariable");
        assert!(matches!(err, RunError::RuntimeError { .. }));
    }

    #[test]
    fn type_error_is_runtime_error() {
        let err = run_err("null.property");
        assert!(matches!(err, RunError::RuntimeError { .. }));
    }

    // ── Export validation ─────────────────────────────────────────────────

    #[test]
    fn exporting_a_function_is_an_error() {
        let err = run_err("export default function() {}");
        assert!(matches!(err, RunError::ExportNotSerializable(_)));
    }

    #[test]
    fn exporting_a_class_is_an_error() {
        let err = run_err("export default class Foo {}");
        assert!(matches!(err, RunError::ExportNotSerializable(_)));
    }

    #[test]
    fn exporting_an_unresolved_promise_is_an_error() {
        // A Promise that is never awaited should be rejected at the boundary.
        let err = run_err("export default new Promise(() => {})");
        assert!(matches!(err, RunError::ExportNotSerializable(_)));
    }

    // ── Complex value structures ───────────────────────────────────────────

    /// Helper: look up a key inside a `WireValue::Object`, returning the value.
    fn wire_obj_get(v: &WireValue, key: &str) -> Option<WireValue> {
        if let WireValue::Object(fields) = v {
            fields.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
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
        assert_eq!(wire_obj_get(&default_val, "result"), Some(WireValue::Number(5050.0)));
        let meta = wire_obj_get(&default_val, "metadata").unwrap();
        assert_eq!(
            wire_obj_get(&meta, "label"),
            Some(WireValue::String("sum_1_to_100".to_string()))
        );
        assert_eq!(wire_obj_get(&meta, "iterations"), Some(WireValue::Number(100.0)));
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
        let out = run_ok(
            r#"export default [{ outer: { inner: 42 } }, { x: [1, 2, 3] }]"#,
        );
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
        let out = run_ok(
            r#"export default { a: [{ b: [{ c: 42 }, { d: "hello" }] }] }"#,
        );
        let default_val = get_field(&out, "default").unwrap();
        let a = wire_obj_get(&default_val, "a").unwrap();
        if let WireValue::Array(a_items) = a {
            let b = wire_obj_get(&a_items[0], "b").unwrap();
            if let WireValue::Array(b_items) = b {
                assert_eq!(wire_obj_get(&b_items[0], "c"), Some(WireValue::Number(42.0)));
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
    fn cyclic_object_is_rejected() {
        let err = run_err(
            r#"
            const obj = { x: 1 };
            obj.self = obj;
            export default obj;
            "#,
        );
        assert!(matches!(err, RunError::ExportNotSerializable(_)));
    }

    #[test]
    fn cyclic_array_is_rejected() {
        let err = run_err(
            r#"
            const arr = [1, 2, 3];
            arr.push(arr);
            export default arr;
            "#,
        );
        assert!(matches!(err, RunError::ExportNotSerializable(_)));
    }

    #[test]
    fn indirect_cycle_array_inside_object_inside_array() {
        // arr → obj → arr (cross-type indirect cycle)
        let err = run_err(
            r#"
            const arr = [];
            const obj = { arr };
            arr.push(obj);
            export default arr;
            "#,
        );
        assert!(matches!(err, RunError::ExportNotSerializable(_)));
    }

    #[test]
    fn indirect_cycle_two_objects() {
        let err = run_err(
            r#"
            const a = {};
            const b = { a };
            a.b = b;
            export default a;
            "#,
        );
        assert!(matches!(err, RunError::ExportNotSerializable(_)));
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
        assert_eq!(wire_obj_get(&wire_obj_get(&default_val, "a").unwrap(), "x"), Some(WireValue::Number(1.0)));
        assert_eq!(wire_obj_get(&wire_obj_get(&default_val, "b").unwrap(), "x"), Some(WireValue::Number(1.0)));
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
        assert_eq!(wire_obj_get(&default_val, "first"), Some(expected_arr.clone()));
        assert_eq!(wire_obj_get(&default_val, "second"), Some(expected_arr));
    }

    // ── BigInt ────────────────────────────────────────────────────────────

    #[test]
    fn bigint_positive_encodes_as_decimal_string() {
        let out = run_ok("export default 42n");
        assert_eq!(get_field(&out, "default"), Some(WireValue::BigInt("42".to_string())));
    }

    #[test]
    fn bigint_negative_encodes_as_decimal_string() {
        let out = run_ok("export default -100n");
        assert_eq!(
            get_field(&out, "default"),
            Some(WireValue::BigInt("-100".to_string()))
        );
    }

    #[test]
    fn bigint_zero_encodes_correctly() {
        let out = run_ok("export default 0n");
        assert_eq!(get_field(&out, "default"), Some(WireValue::BigInt("0".to_string())));
    }

    #[test]
    fn bigint_inside_object_encodes_correctly() {
        let out = run_ok("export default { count: 1000000000000n }");
        let default_val = get_field(&out, "default").unwrap();
        assert_eq!(
            wire_obj_get(&default_val, "count"),
            Some(WireValue::BigInt("1000000000000".to_string()))
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
        assert!(matches!(err, RunError::RuntimeError { .. }));
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
    // Requires the module resolver (Phase 6+).

    #[test]
    fn unknown_import_is_module_not_found() {
        let err = run_err(r#"import { foo } from "unknown:module"; export default foo"#);
        assert!(matches!(err, RunError::ModuleNotFound(_)));
    }

    #[test]
    fn source_import_provided_by_host_works() {
        // Host provides the source for "math:add". Once imports are wired up
        // this should resolve and run the provided source.
        let err = run_err(
            r#"
            import { add } from "math:add";
            export default add(1, 2)
            "#,
        );
        // For now it fails with ModuleNotFound because no resolver exists.
        // Once Phase 6 lands this test body will change to assert Ok(3).
        assert!(matches!(err, RunError::ModuleNotFound(_)));
    }

    #[test]
    fn host_module_function_callable() {
        // Host provides a function under "host:tools". Phase 7+.
        let err = run_err(
            r#"
            import { search } from "host:tools";
            export default await search("query")
            "#,
        );
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
            Limits { cpu_time_ms: 200, wall_time_ms: 1_000, ..Default::default() },
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
            Limits { cpu_time_ms: 0, wall_time_ms: 0, ..Default::default() },
        ).unwrap();
        assert_eq!(get_default(&out).as_deref(), Some("42"));
    }

    #[test]
    fn run_within_budget_succeeds() {
        let out = run_code(
            "let s = 0; for (let i = 0; i < 10_000; i++) s += i; export default s",
            "<test>",
            Limits { cpu_time_ms: 5_000, wall_time_ms: 10_000, ..Default::default() },
        ).unwrap();
        assert_eq!(get_default(&out).as_deref(), Some("49995000"));
    }

    #[test]
    fn execute_with_prefix_infinite_loop_is_killed() {
        let snapshot = precompile("globalThis.base = 10", None).unwrap();
        let err = execute_with_prefix(
            &snapshot,
            "while (true) {}",
            None,
            Limits { cpu_time_ms: 200, wall_time_ms: 1_000, ..Default::default() },
            &[],
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
            Limits { cpu_time_ms: 200, wall_time_ms: 1_000, ..Default::default() },
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
            Limits { cpu_time_ms: 5_000, wall_time_ms: 10_000, ..Default::default() },
        )
        .map_err(|f| f.error)
        .unwrap_err();
        assert!(
            matches!(err, RunError::RuntimeError { .. }),
            "expected RuntimeError (stack overflow), got {err:?}"
        );
    }

    #[test]
    fn very_tight_wall_limit_fires() {
        let err = run_code(
            "while (true) {}",
            "<test>",
            Limits { cpu_time_ms: 30_000, wall_time_ms: 1, ..Default::default() },
        )
        .map_err(|f| f.error)
        .unwrap_err();
        assert!(
            matches!(err, RunError::WallTimeout | RunError::CpuTimeout),
            "expected timeout, got {err:?}"
        );
    }

    #[test]
    #[ignore = "requires memory limit (Phase 8)"]
    fn allocating_too_much_memory_is_memory_limit() {
        let err = run_err(
            r#"
            const arrays = [];
            while (true) { arrays.push(new Uint8Array(1024 * 1024)); }
            "#,
        );
        assert!(matches!(err, RunError::MemoryLimit));
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
            Limits { wall_time_ms: 200, cpu_time_ms: 30_000, ..Default::default() },
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

    #[test]
    fn source_module_basic_function_import() {
        // Host provides source for "lib:math". The module exports an `add`
        // function. User code imports and calls it.
        //
        // When Phase 6 lands: wire up a source import for "lib:math" that
        // contains `export function add(a, b) { return a + b; }` and assert
        // the result is 3.
        let err = run_err(
            r#"
            import { add } from "lib:math";
            export default add(1, 2)
            "#,
        );
        assert!(matches!(err, RunError::ModuleNotFound(_)));
    }

    #[test]
    fn source_module_mimicking_zod_schema_validation() {
        // The host provides a simplified zod-like schema library as source.
        // This is the canonical use-case: precompile the library into the
        // prefix snapshot once, then validate data in every postfix run.
        //
        // Mock source the host would supply for "lib:zod":
        //
        //   export const z = {
        //     object: (shape) => ({
        //       parse: (data) => {
        //         for (const key of Object.keys(shape)) {
        //           if (!(key in data)) throw new Error(`missing key: ${key}`);
        //         }
        //         return data;
        //       }
        //     }),
        //     string: () => ({}),
        //     number: () => ({}),
        //   };
        //
        // When Phase 6 lands: provide that source, assert parse succeeds.
        let err = run_err(
            r#"
            import { z } from "lib:zod";
            const schema = z.object({ name: z.string(), age: z.number() });
            export default schema.parse({ name: "Alice", age: 30 })
            "#,
        );
        assert!(matches!(err, RunError::ModuleNotFound(_)));
    }

    #[test]
    fn source_module_zod_schema_fails_on_bad_data() {
        // Same setup as above, but pass data that fails validation.
        // When Phase 6 lands: assert this surfaces as RuntimeError (the
        // schema throws), not ModuleNotFound.
        let err = run_err(
            r#"
            import { z } from "lib:zod";
            const schema = z.object({ name: z.string() });
            export default schema.parse({ wrong: true })
            "#,
        );
        // Pre-phase-6: ModuleNotFound. Post-phase-6: RuntimeError.
        assert!(matches!(
            err,
            RunError::ModuleNotFound(_) | RunError::RuntimeError { .. }
        ));
    }

    #[test]
    fn source_module_utility_library_used_across_multiple_exports() {
        // A utility lib is imported and used in both the default and a named
        // export. Verifies the module is only compiled once and shared.
        let err = run_err(
            r#"
            import { clamp, lerp } from "lib:math-utils";
            export default clamp(5, 0, 10);
            export const interpolated = lerp(0, 100, 0.5);
            "#,
        );
        assert!(matches!(err, RunError::ModuleNotFound(_)));
    }

    #[test]
    fn source_module_can_import_another_source_module() {
        // Transitive source imports: "lib:app" imports from "lib:utils".
        // Phase 6+: module resolver must handle transitive resolution.
        let err = run_err(
            r#"
            import { formatResult } from "lib:app";
            export default formatResult(42)
            "#,
        );
        assert!(matches!(err, RunError::ModuleNotFound(_)));
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
        assert!(matches!(err, RunError::RuntimeError { .. }));
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
        assert!(matches!(err, RunError::RuntimeError { .. }));
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
        assert!(out.duration_ms < 5_000);
    }

    // ── Precompile / snapshots ──────────────────────────────────────────────

    #[test]
    fn precompile_returns_non_empty_snapshot_bytes() {
        let bytes = precompile("const x = 1", None).unwrap();
        assert!(!bytes.is_empty());
    }

    #[test]
    fn precompile_compile_error_is_reported() {
        let err = precompile("export default (((", None).unwrap_err();
        assert!(matches!(err.error, RunError::CompileError(_)));
    }

    #[test]
    fn precompile_runtime_error_is_reported() {
        let err = precompile(r#"throw new Error("prefix failed")"#, None).unwrap_err();
        assert!(matches!(err.error, RunError::RuntimeError { .. }));
    }

    #[test]
    fn execute_with_prefix_basic_postfix() {
        // Module-scoped `const` stays in the prefix module's scope.
        // Use globalThis to share values with the postfix module.
        let snapshot = precompile("globalThis.base = 100", None).unwrap();
        let out = execute_with_prefix(&snapshot, "export default globalThis.base + 1", None, Limits::default(), &[], None)
            .unwrap();
        assert_eq!(get_default(&out).as_deref(), Some("101"));
    }

    #[test]
    fn execute_with_prefix_global_mutation_visible_in_postfix() {
        let snapshot = precompile("globalThis.answer = 42", None).unwrap();
        let out = execute_with_prefix(&snapshot, "export default globalThis.answer", None, Limits::default(), &[], None).unwrap();
        assert_eq!(get_default(&out).as_deref(), Some("42"));
    }

    #[test]
    fn execute_with_prefix_multiple_postfixes_are_independent() {
        let snapshot = precompile("globalThis.base = 10", None).unwrap();
        let b = "globalThis.base";
        let out1 = execute_with_prefix(&snapshot, &format!("export default {b} * 2"), None, Limits::default(), &[], None).unwrap();
        let out2 = execute_with_prefix(&snapshot, &format!("export default {b} * 3"), None, Limits::default(), &[], None).unwrap();
        let out3 = execute_with_prefix(&snapshot, &format!("export default {b} * 4"), None, Limits::default(), &[], None).unwrap();
        assert_eq!(get_default(&out1).as_deref(), Some("20"));
        assert_eq!(get_default(&out2).as_deref(), Some("30"));
        assert_eq!(get_default(&out3).as_deref(), Some("40"));
    }

    #[test]
    fn execute_with_prefix_postfix_mutations_do_not_leak_between_runs() {
        let snapshot = precompile("globalThis.counter = 0", None).unwrap();
        execute_with_prefix(&snapshot, "globalThis.counter = 99; export default 1", None, Limits::default(), &[], None).unwrap();
        let out = execute_with_prefix(&snapshot, "export default globalThis.counter", None, Limits::default(), &[], None).unwrap();
        assert_eq!(get_default(&out).as_deref(), Some("0"));
    }

    #[test]
    fn execute_with_prefix_complex_prefix_computation() {
        let snapshot = precompile(
            r#"const sq = {}; for (let i = 0; i <= 10; i++) sq[i] = i * i; globalThis.sq = sq;"#,
            None,
        ).unwrap();
        let out = execute_with_prefix(&snapshot, "export default globalThis.sq[7]", None, Limits::default(), &[], None).unwrap();
        assert_eq!(get_default(&out).as_deref(), Some("49"));
    }

    #[test]
    fn execute_with_prefix_console_is_available_in_postfix() {
        let snapshot = precompile("const x = 1", None).unwrap();
        let out = execute_with_prefix(
            &snapshot,
            r#"console.log("hello from postfix"); export default 1"#,
            None,
            Limits::default(),
            &[],
            None,
        ).unwrap();
        assert!(out.stdout.iter().any(|l| l.contains("hello from postfix")));
    }

    #[test]
    fn execute_with_prefix_postfix_runtime_error_is_reported() {
        let snapshot = precompile("", None).unwrap();
        let err = execute_with_prefix(&snapshot, r#"throw new Error("postfix failed")"#, None, Limits::default(), &[], None)
            .unwrap_err();
        assert!(matches!(err.error, RunError::RuntimeError { .. }));
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
    #[ignore = "requires maxExportBytes enforcement"]
    fn export_exceeding_size_limit_is_err_export_too_large() {
        // User code exports a very large string/array. The serialized export
        // bytes exceed `maxExportBytes`; the run should fail instead of
        // sending a huge payload over the wire.
        let err = run_err(
            r#"
            export default "x".repeat(32 * 1024 * 1024)
            "#,
        );
        assert!(matches!(err, RunError::ExportNotSerializable(_)));
        // TODO: add an ExportTooLarge variant and check that instead.
    }

    // ── stdout / stderr size limits ───────────────────────────────────────
    // Requires maxStdoutBytes / maxStderrBytes enforcement (Phase 3+).

    #[test]
    #[ignore = "requires maxStdoutBytes enforcement (Phase 3)"]
    fn stdout_exceeding_limit_truncates_or_errors() {
        // Writing more than maxStdoutBytes via console.log must not OOM the
        // process. Either truncate silently or fail with an internal error.
        // The current design leans toward truncation (cap, not error).
        todo!()
    }

    #[test]
    #[ignore = "requires maxStderrBytes enforcement (Phase 3)"]
    fn stderr_exceeding_limit_truncates_or_errors() {
        todo!()
    }

    // ── Host bridge error propagation ─────────────────────────────────────
    // Phase 7+.

    #[test]
    #[ignore = "requires host import bridge (Phase 7)"]
    fn host_import_function_throws_is_err_host_import() {
        // A host-provided import function throws. The error must surface as
        // ERR_HOST_BRIDGE in the result, not crash the runtime.
        todo!()
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
            wire::encode_wire_value(&WireValue::Number(1.0), &mut payload);
            ipc::write_ts_to_rust_frame(
                &mut server,
                ipc::TsToRustMessageType::BridgeResponse,
                &payload,
            ).unwrap();
        });

        let out = execute(
            "export default await myTool()",
            None,
            Limits { cpu_time_ms: 50, wall_time_ms: 5_000, ..Default::default() },
            &["myTool".to_string()],
            Some(fd),
        );
        responder.join().unwrap();
        assert!(out.is_ok(), "expected Ok (bridge wait excluded from cpu), got: {:?}",
            out.map_err(|f| f.error));
    }

    #[test]
    fn cpu_budget_does_count_tight_sync_loop() {
        let err = run_code(
            "let x = 0; while (true) x++;",
            "<test>",
            Limits { cpu_time_ms: 100, wall_time_ms: 5_000, ..Default::default() },
        ).map_err(|f| f.error).unwrap_err();
        assert!(matches!(err, RunError::CpuTimeout),
            "expected CpuTimeout, got {err:?}");
    }

    // ── Bridge tests ───────────────────────────────────────────────────────────────────
    //
    // These tests exercise the full bridge round-trip: V8 calls a host global,
    // a responder thread reads the BridgeCall frame and writes a BridgeResponse,
    // and we assert on the returned JS value or error code.

    /// Encode a successful BridgeResponse payload (callId, ok=1, value).
    fn bridge_resp_ok(call_id: u32, value: &WireValue) -> Vec<u8> {
        let mut p = Vec::new();
        p.extend_from_slice(&call_id.to_be_bytes());
        p.push(1); // ok
        p.push(1); // value present
        wire::encode_wire_value(value, &mut p);
        p
    }

    /// Encode an error BridgeResponse payload (callId, ok=0, message).
    fn bridge_resp_err(call_id: u32, message: &str) -> Vec<u8> {
        let mut p = Vec::new();
        p.extend_from_slice(&call_id.to_be_bytes());
        p.push(0); // ok = false
        for s in &["ERR_HOST_BRIDGE", "Error", message] {
            let b = s.as_bytes();
            p.extend_from_slice(&(b.len() as u32).to_be_bytes());
            p.extend_from_slice(b);
        }
        p.push(0); // no stack
        p
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
            code, None, limits, &[global.to_string()], Some(fd),
        );
        (result, handle)
    }

    #[test]
    fn bridge_call_returns_number() {
        let (out, h) = run_with_bridge(
            "export default await myTool()",
            "myTool",
            Limits { cpu_time_ms: 5_000, wall_time_ms: 10_000, ..Default::default() },
            |s| {
                ipc::write_ts_to_rust_frame(s, ipc::TsToRustMessageType::BridgeResponse,
                    &bridge_resp_ok(0, &WireValue::Number(42.0))).unwrap();
            },
        );
        h.join().unwrap();
        assert_eq!(get_default(&out.unwrap()).as_deref(), Some("42"));
    }

    #[test]
    fn bridge_call_returns_string() {
        let (out, h) = run_with_bridge(
            "export default await myTool()",
            "myTool",
            Limits { cpu_time_ms: 5_000, wall_time_ms: 10_000, ..Default::default() },
            |s| {
                ipc::write_ts_to_rust_frame(s, ipc::TsToRustMessageType::BridgeResponse,
                    &bridge_resp_ok(0, &WireValue::String("hello bridge".into()))).unwrap();
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
            // callId (4) + targetKind (1) + specifier absent (1) + name len (4) + "add" (3) + argCount (4) = 17
            // Then first arg: tag NUMBER (1) + f64 (8) = 9
            // Spot-check the arg count field (offset 13)
            let arg_count = u32::from_be_bytes(frame.payload[13..17].try_into().unwrap());
            assert_eq!(arg_count, 1);
            ipc::write_ts_to_rust_frame(&mut server, ipc::TsToRustMessageType::BridgeResponse,
                &bridge_resp_ok(0, &WireValue::Number(99.0))).unwrap();
        });

        let out = execute(
            "export default await add(7)",
            None,
            Limits { cpu_time_ms: 5_000, wall_time_ms: 10_000, ..Default::default() },
            &["add".to_string()],
            Some(fd),
        ).unwrap();
        responder.join().unwrap();
        assert_eq!(get_default(&out).as_deref(), Some("99"));
    }

    #[test]
    fn bridge_call_host_error_surfaces_as_host_bridge_error() {
        let (result, h) = run_with_bridge(
            "export default await myTool()",
            "myTool",
            Limits { cpu_time_ms: 5_000, wall_time_ms: 10_000, ..Default::default() },
            |s| {
                ipc::write_ts_to_rust_frame(s, ipc::TsToRustMessageType::BridgeResponse,
                    &bridge_resp_err(0, "handler blew up")).unwrap();
            },
        );
        h.join().unwrap();
        let err = result.unwrap_err().error;
        assert!(matches!(err, RunError::HostBridge(ref m) if m.contains("handler blew up")),
            "expected HostBridge, got {err:?}");
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
        server.set_read_timeout(Some(Duration::from_millis(500))).ok();
        let responder = std::thread::spawn(move || {
            let _ = ipc::read_rust_to_ts_frame(&mut server); // may timeout — ignore
        });

        let err = execute(
            "export default await myTool(() => 42)",
            None,
            Limits { cpu_time_ms: 5_000, wall_time_ms: 10_000, ..Default::default() },
            &["myTool".to_string()],
            Some(fd),
        ).unwrap_err().error;

        responder.join().unwrap();
        assert!(matches!(err, RunError::FunctionArgumentNotSupported),
            "expected FunctionArgumentNotSupported, got {err:?}");
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
            Limits { cpu_time_ms: 10_000, wall_time_ms: 300, ..Default::default() },
            &["myTool".to_string()],
            Some(fd),
        ).unwrap_err().error;

        assert!(matches!(err, RunError::WallTimeout),
            "expected WallTimeout, got {err:?}");
    }

    #[test]
    fn bridge_call_returns_object() {
        let (out, h) = run_with_bridge(
            "const r = await myTool(); export default r.x + r.y",
            "myTool",
            Limits { cpu_time_ms: 5_000, wall_time_ms: 10_000, ..Default::default() },
            |s| {
                ipc::write_ts_to_rust_frame(s, ipc::TsToRustMessageType::BridgeResponse,
                    &bridge_resp_ok(0, &WireValue::Object(vec![
                        ("x".to_string(), WireValue::Number(3.0)),
                        ("y".to_string(), WireValue::Number(4.0)),
                    ]))).unwrap();
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
            Limits { wall_time_ms: 200, cpu_time_ms: 30_000, ..Default::default() },
            &["myTool".to_string()],
            Some(fd),
        ).unwrap_err().error;
        assert!(matches!(err, RunError::WallTimeout), "got {err:?}");
    }

    // ── __proto__ elision ─────────────────────────────────────────────────────

    #[test]
    fn bridge_proto_key_in_host_response_is_dropped() {
        // Host returns an object that contains "__proto__" as a key.
        // wire_to_v8_value must drop it: the sandbox must NOT see __proto__
        // as an own property of the returned object.
        let (out, h) = run_with_bridge(
            // Sort own property names to get a deterministic comma-joined string.
            "const r = await myTool(); \
             export default Object.getOwnPropertyNames(r).sort().join(',')",
            "myTool",
            Limits { cpu_time_ms: 5_000, wall_time_ms: 10_000, ..Default::default() },
            |s| {
                ipc::write_ts_to_rust_frame(
                    s,
                    ipc::TsToRustMessageType::BridgeResponse,
                    &bridge_resp_ok(0, &WireValue::Object(vec![
                        ("x".to_string(),         WireValue::Number(1.0)),
                        ("__proto__".to_string(), WireValue::Number(99.0)),
                        ("y".to_string(),         WireValue::Number(2.0)),
                    ])),
                ).unwrap();
            },
        );
        h.join().unwrap();
        // "__proto__" was dropped; only "x" and "y" survive as own properties.
        assert_eq!(get_default(&out.unwrap()).as_deref(), Some("x,y"));
    }

    #[test]
    fn sandbox_export_with_proto_own_property_is_dropped() {
        // Sandbox creates an object with __proto__ as an explicit own
        // enumerable property via Object.defineProperty (a plain object
        // literal `{ __proto__: x }` sets the prototype instead).
        // serialize_object_fields must drop it before crossing to the host.
        let out = run_ok(r#"
            const obj = {};
            Object.defineProperty(obj, '__proto__', { value: 99, enumerable: true });
            obj.x = 1;
            export default obj;
        "#);
        let default_val = get_field(&out, "default").unwrap();
        if let WireValue::Object(fields) = default_val {
            let keys: Vec<&str> = fields.iter().map(|(k, _)| k.as_str()).collect();
            assert!(
                !keys.contains(&"__proto__"),
                "expected __proto__ to be dropped, got keys: {keys:?}"
            );
            assert!(keys.contains(&"x"), "expected x to survive");
        } else {
            panic!("expected WireValue::Object");
        }
    }
}
