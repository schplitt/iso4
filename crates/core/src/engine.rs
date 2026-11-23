//! The engine as a frontend sees it: one [`Engine`] per process owns the
//! prefix store and the warm registry. It answers through the caller's
//! [`HostLink`] and never sees a frame.

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use crate::container;
use crate::errors;
use crate::link::{HostLink, Inbound, Observer, Outbound};
use crate::prefix::{validate_global_rebinds, validate_import_rebinds, PrefixData, PrefixEntry};
use crate::types;
use crate::v8 as sandbox;
use crate::warm;

/// Per-run trace lines (`Run`/`PrefixRun` received / succeeded / failed) are
/// off by default: two stderr lines per run cost 2-4 % of a hot
/// `prefix.call()`. `ISO4_V8_TRACE=1` turns them on.
#[doc(hidden)]
pub fn trace_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        matches!(
            std::env::var("ISO4_V8_TRACE").as_deref(),
            Ok("1") | Ok("true")
        )
    })
}

/// [`log!`](crate::log!) gated on [`trace_enabled`]; arguments are only evaluated when
/// tracing is on.
#[macro_export]
macro_rules! trace {
    ($($arg:tt)*) => {
        if $crate::trace_enabled() {
            $crate::log!($($arg)*);
        }
    };
}

/// What a frontend hands every run it starts.
pub struct Caller {
    pub link: Arc<dyn HostLink>,
    pub observer: Arc<dyn Observer>,
    /// Bridge call ids, unique across all of this caller's runs.
    pub call_ids: Arc<AtomicU32>,
    /// The key host-type descriptors from this caller must carry.
    pub brand_key: String,
}

/// A run refused before it started: nothing ran, nothing is held.
#[derive(Debug)]
pub enum Refusal {
    /// The memory admission line.
    Capacity(String),
    /// No such prefix: disposed, or never prepared.
    PrefixDisposed(String),
    /// The prefix's cold start failed; `cause` is that failure.
    PrefixBroken {
        prefix_id: String,
        cause: Arc<types::RunErrorPayload>,
    },
    /// A rebind the prefix did not declare, or of a frozen binding.
    Binding(sandbox::RunError),
    /// The runtime could not start the run, or the caller erred (a duplicate run id).
    Internal(String),
}

impl Refusal {
    pub fn to_payload(&self) -> types::RunErrorPayload {
        let plain = |code: &str, message: String, cause| types::RunErrorPayload {
            code: code.to_string(),
            name: "Error".to_string(),
            message,
            stack: None,
            fields: None,
            reset: None,
            prefix_stage: false,
            cause,
        };
        match self {
            Refusal::Capacity(refusal) => sandbox::RunError::Capacity(refusal.clone()).to_payload(),
            Refusal::Internal(detail) => sandbox::RunError::Internal(detail.clone()).to_payload(),
            Refusal::Binding(error) => error.to_payload(),
            Refusal::PrefixDisposed(prefix_id) => plain(
                "ERR_PREFIX_DISPOSED",
                format!("prefix '{prefix_id}' has been disposed or never existed"),
                None,
            ),
            Refusal::PrefixBroken { prefix_id, cause } => plain(
                "ERR_PREFIX_BROKEN",
                format!(
                    "prefix '{prefix_id}' is broken: its cold start failed with {}: {}",
                    cause.code, cause.message
                ),
                Some(Arc::clone(cause)),
            ),
        }
    }
}

/// A started run as its frontend holds it: where the run's inbound
/// messages go. Handed to `register` before the run can receive anything;
/// deliver only after `run`/`prefix_run` returned `Ok`, and drop it on `Err`.
#[derive(Clone)]
pub struct RunHandle {
    token: u64,
    run_id: u32,
    /// A one-off run's private channel, or its warm instance's shared one.
    events: sandbox::RunEventSender,
    /// The owning instance guard's cross-thread face, filled once the
    /// isolate exists; the mid-turn target of a hard Terminate.
    ctl: Arc<OnceLock<sandbox::GuardCtl>>,
}

impl RunHandle {
    /// Send one message to the run; dropped if the run has finished. A hard
    /// Terminate kills the run's turn if it is executing right now;
    /// otherwise it is delivered like any message and abandons the run at
    /// its next turn boundary.
    pub fn deliver(&self, msg: Inbound) {
        if let Inbound::Terminate {
            mode: types::TerminateMode::Hard,
        } = msg
        {
            if let Some(ctl) = self.ctl.get() {
                if ctl.abort_executing(self.token) {
                    crate::log!(
                        "[iso4-v8] hard Terminate for run {} landed mid-turn — terminating",
                        self.run_id
                    );
                    return;
                }
            }
        }
        self.events
            .send(sandbox::RoutedEvent::new(
                self.token,
                sandbox::RunEvent::Host(msg),
            ))
            .ok();
    }
}

/// Process-wide settings for [`Engine::start`]. `Default` holds every default.
#[derive(Clone, Debug)]
pub struct EngineOptions {
    /// `None` = the machine's count. Sizes V8's thread pool and the per-prefix ceiling.
    pub cores: Option<NonZeroUsize>,
    /// Called from a dying process on a fatal V8 error; `None` = V8 prints its own.
    pub on_fatal: Option<sandbox::FatalHook>,
    /// The mark the warm registry sheds against (global container usage); 0 = off.
    pub warm_budget_bytes: u64,
    /// The isolate admission line.
    pub admission_line: AdmissionLine,
    /// What the container limit owes the host before [`AdmissionLine::Auto`] is drawn.
    pub host_reserve_bytes: u64,
    /// Idle seconds before an instance settles its heap once; 0 = off.
    pub idle_settle_secs: u64,
}

impl Default for EngineOptions {
    fn default() -> Self {
        Self {
            cores: None,
            on_fatal: None,
            warm_budget_bytes: 0,
            admission_line: AdmissionLine::Auto,
            host_reserve_bytes: container::DEFAULT_HOST_RESERVE_BYTES,
            idle_settle_secs: 0,
        }
    }
}

/// Where isolate admission stops.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdmissionLine {
    /// 90% of (container limit - host reserve); off when no limit is readable.
    Auto,
    /// No line.
    Off,
    /// This many bytes.
    Bytes(u64),
}

/// Why [`Engine::start`] refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum StartError {
    /// `start` already ran in this process, or V8 was started before it.
    AlreadyStarted,
    /// A warm budget is set, but neither container memory nor this process's
    /// resident memory is readable.
    NoMemoryMeter,
}

impl std::fmt::Display for StartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::AlreadyStarted => "the engine was already started in this process",
            Self::NoMemoryMeter => {
                "no memory meter is readable, so the memory budget could never be enforced"
            }
        })
    }
}

impl std::error::Error for StartError {}

/// The memory lines an engine runs under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capacity {
    /// The container's memory limit; `None` when unreadable.
    pub container_limit_bytes: Option<u64>,
    /// Where the limit was read from ("cgroup v2", "cgroup v1", "host total").
    pub limit_source: Option<&'static str>,
    pub host_reserve_bytes: u64,
    /// 0 = no line.
    pub admission_line_bytes: u64,
}

/// One per process. A prefix prepared through any caller is visible to all
/// of them, so the host pool can route any run to any connection. A caller
/// starts its runs from one thread.
pub struct Engine {
    prefixes: Mutex<HashMap<String, PrefixEntry>>,
    next_prefix_id: AtomicU64,
    /// Every prefix run is served by a resident instance from here; one-off
    /// runs share its slot accounting.
    warm: warm::WarmRegistry,
    admission_line_bytes: u64,
}

impl Engine {
    /// Once per process: checks the memory meter, stores the settings, starts
    /// V8, runs the blob probe and seeds the slot allowance.
    pub fn start(options: EngineOptions) -> Result<Engine, StartError> {
        // A budget with no readable meter would silently never be enforced
        // (DESIGN.md §13.2.1). Checked first: it stores nothing, so a retry works.
        let budget = options.warm_budget_bytes;
        if budget > 0
            && container::usage_bytes().is_none()
            && crate::rss::process_rss_bytes().is_none()
        {
            return Err(StartError::NoMemoryMeter);
        }

        // Decided inside V8's one start, so a lazy start on another thread
        // cannot slip in between.
        let started = sandbox::init_platform_with(|| {
            if let Some(hook) = options.on_fatal {
                sandbox::set_fatal_hook(hook);
            }
            if let Some(n) = options.cores {
                sandbox::set_cores(n);
            }
            container::set_host_reserve_bytes(options.host_reserve_bytes);
            sandbox::set_idle_settle_secs(options.idle_settle_secs);
        });
        if !started {
            return Err(StartError::AlreadyStarted);
        }
        // The handshake then compares bytes: no isolate per connection.
        crate::blob::probe();
        // Cold start: one run per core until a completed run reports its shape.
        warm::seed_slot_allowance(warm::cores());

        let line = match options.admission_line {
            AdmissionLine::Auto => container::admission_line_bytes(),
            AdmissionLine::Off => 0,
            AdmissionLine::Bytes(n) => n,
        };
        Ok(Self::new(budget, line))
    }

    /// For tests that start V8 lazily (`testing::engine`).
    pub(crate) fn new(warm_budget_bytes: u64, hard_line_bytes: u64) -> Self {
        Self {
            prefixes: Mutex::new(HashMap::new()),
            next_prefix_id: AtomicU64::new(0),
            warm: warm::WarmRegistry::new(warm_budget_bytes, hard_line_bytes),
            admission_line_bytes: hard_line_bytes,
        }
    }

    /// The container limit, host reserve and admission line in force.
    pub fn capacity(&self) -> Capacity {
        let limit = container::limit_info();
        Capacity {
            container_limit_bytes: limit.map(|(bytes, _)| bytes),
            limit_source: limit.map(|(_, source)| source),
            host_reserve_bytes: container::host_reserve_bytes(),
            admission_line_bytes: self.admission_line_bytes,
        }
    }

    /// Runs the host may keep in flight now; 0 = no opinion yet. Process-wide:
    /// one engine per process.
    pub fn slot_allowance(&self) -> u32 {
        warm::slot_allowance()
    }

    /// Answer a run refused before it started: one bare failure on the caller's
    /// link. Touches no route, so a refused duplicate id leaves the live run alone.
    pub fn refuse(&self, caller: &Caller, run_id: u32, refusal: Refusal) {
        let completion =
            types::RunCompletion::Failure(types::RunFailurePayload::bare(refusal.to_payload()));
        sandbox::send_completion(&*caller.link, run_id, Outbound::Result { completion });
    }

    /// Store a prefix and return its id. Nothing is evaluated: a broken
    /// prefix fails its first run instead.
    pub fn prepare(
        &self,
        code: String,
        filename: Option<String>,
        globals: Vec<types::HostGlobalDef>,
        imports: Vec<types::ImportBinding>,
    ) -> String {
        let id = self.next_prefix_id.fetch_add(1, Ordering::Relaxed);
        let prefix_id = id.to_string();
        // Only bridge stubs are rebindable per run; value globals and shim
        // wrappers are replayed from the stored defs.
        let declared_globals: Vec<String> = globals
            .iter()
            .filter_map(|g| g.bridge_stub_name().map(str::to_string))
            .collect();
        crate::log!(
            "[iso4-v8] prefix stored — prefix_id={prefix_id} \
             ({} source bytes, {} declared globals, {} declared imports)",
            code.len(),
            declared_globals.len(),
            imports.len(),
        );
        self.prefixes
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(
                prefix_id.clone(),
                PrefixEntry::Ready(Arc::new(PrefixData {
                    id,
                    code,
                    filename,
                    globals,
                    declared_globals,
                    declared_imports: imports,
                })),
            );
        prefix_id
    }

    /// Forget a prefix and drop its idle instances. Busy instances finish
    /// their runs and are dropped instead of pooled.
    pub fn dispose_prefix(&self, prefix_id: &str) {
        // The store lock spans the warm dispose so it is atomic with a
        // concurrent release's aliveness check (lock order prefixes → warm).
        let existed = {
            let mut store = self.prefixes.lock().unwrap_or_else(|p| p.into_inner());
            let existed = store.remove(prefix_id).is_some();
            self.warm.dispose_prefix(prefix_id);
            existed
        };
        crate::log!("[iso4-v8] DisposePrefix prefix_id={prefix_id} (existed={existed})");
    }

    pub fn stats(&self) -> warm::RegistryStats {
        self.warm.stats()
    }

    /// A one-off run on its own worker thread and a fresh isolate. The
    /// answer goes out on the caller's link.
    pub fn run(
        self: &Arc<Self>,
        req: types::RunRequest,
        caller: &Caller,
        register: &mut dyn FnMut(RunHandle),
    ) -> Result<(), Refusal> {
        trace!(
            "[iso4-v8] Run {} received ({} code bytes, {} globals, call={})",
            req.run_id,
            req.code.len(),
            req.globals.len(),
            req.call
                .as_ref()
                .map(|c| c.export_path.as_str())
                .unwrap_or("-"),
        );
        let run_id = req.run_id;
        let run_cap_bytes = u64::from(req.limits.hard_memory_mb) * 1024 * 1024;
        self.warm
            .reserve_oneoff(run_cap_bytes)
            .map_err(Refusal::Capacity)?;

        let token = sandbox::alloc_run_token();
        let ctl = Arc::new(OnceLock::new());
        // A one-off run gets a private event channel (it owns no instance loop).
        let (events_tx, events) = crossbeam_channel::unbounded();
        register(RunHandle {
            token,
            run_id,
            events: events_tx,
            ctl: Arc::clone(&ctl),
        });

        let engine = Arc::clone(self);
        let link = Arc::clone(&caller.link);
        let observer = Arc::clone(&caller.observer);
        let call_ids = Arc::clone(&caller.call_ids);
        let brand_key = caller.brand_key.clone();
        let spawned = std::thread::Builder::new()
            .name("iso4-oneoff-run".to_string())
            .spawn(move || {
                crate::webcodec::set_thread_brand_key(brand_key);
                let result = sandbox::execute_with_io(
                    &req.code,
                    req.filename.as_deref(),
                    sandbox::Limits::from(&req.limits),
                    &req.globals,
                    &req.imports,
                    sandbox::RunIo::Channel {
                        events,
                        link: Arc::clone(&link),
                    },
                    token,
                    call_ids,
                    req.call.as_ref(),
                    Some(ctl),
                    observer,
                    Some(sandbox::EpilogueSpec {
                        run_id,
                        report_heap: false,
                    }),
                );
                match &result {
                    Ok(output) => {
                        trace!("[iso4-v8] run succeeded in {:.3}ms", output.duration_ms)
                    }
                    Err(failure) => trace!(
                        "[iso4-v8] run failed in {:.3}ms: {:?}",
                        failure.duration_ms,
                        failure.error
                    ),
                }
                let (wall_time_ms, cpu_time_ms) = match &result {
                    Ok(output) => (output.wall_time_ms, output.cpu_time_ms),
                    Err(failure) => (failure.wall_time_ms, failure.cpu_time_ms),
                };
                sandbox::finish_run(
                    Some(&*link),
                    run_id,
                    Some(sandbox::completion_message(&result, None, false)),
                    || {
                        engine
                            .warm
                            .release_oneoff(result.is_err(), wall_time_ms, cpu_time_ms)
                    },
                );
            });
        if let Err(e) = spawned {
            crate::log!("[iso4-v8] failed to spawn one-off worker: {e}");
            // Never ran: no shape to learn from.
            self.warm.release_oneoff(true, 0.0, 0.0);
            return Err(Refusal::Internal(format!(
                "failed to spawn run worker: {e}"
            )));
        }
        Ok(())
    }

    /// A call into a prepared prefix, served by a warm instance: reused
    /// idle, joined busy, or spawned. The answer goes out on the caller's
    /// link once the instance is back in the registry.
    pub fn prefix_run(
        self: &Arc<Self>,
        req: types::PrefixRunRequest,
        caller: &Caller,
        register: &mut dyn FnMut(RunHandle),
    ) -> Result<(), Refusal> {
        let run_id = req.run_id;

        // An `Arc` handle: the store lock is held for one refcount bump.
        let entry = self
            .prefixes
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&req.prefix_id)
            .map(|entry| match entry {
                PrefixEntry::Ready(data) => Ok(Arc::clone(data)),
                PrefixEntry::Broken(cause) => Err(Arc::clone(cause)),
            });
        let prefix_data = match entry {
            Some(Ok(data)) => data,
            Some(Err(cause)) => {
                crate::log!(
                    "[iso4-v8] PrefixRun {run_id} — ERR_PREFIX_BROKEN: prefix_id={} cold start \
                     failed with {}: {}",
                    req.prefix_id, cause.code, cause.message
                );
                return Err(Refusal::PrefixBroken {
                    prefix_id: req.prefix_id,
                    cause,
                });
            }
            None => {
                crate::log!(
                    "[iso4-v8] PrefixRun {} — prefix_id={} not found",
                    run_id, req.prefix_id
                );
                return Err(Refusal::PrefixDisposed(req.prefix_id));
            }
        };

        // Globals first, then import rebinds; the TS client runs the same
        // checks before sending.
        let violation = validate_global_rebinds(
            &req.globals,
            &prefix_data.declared_globals,
            &prefix_data.globals,
        )
        .err()
        .or_else(|| {
            validate_import_rebinds(&req.import_rebinds, &prefix_data.declared_imports).err()
        });
        if let Some(error) = violation {
            let payload = error.to_payload();
            crate::log!(
                "[iso4-v8] PrefixRun {run_id} — {}: {}",
                payload.code, payload.message
            );
            return Err(Refusal::Binding(error));
        }

        trace!(
            "[iso4-v8] PrefixRun {} (prefix_id={}, {} globals, {} imports, call={})",
            run_id,
            req.prefix_id,
            req.globals.len(),
            prefix_data.declared_imports.len(),
            req.call.export_path,
        );

        let token = sandbox::alloc_run_token();
        let ctl = Arc::new(OnceLock::new());

        // Cold = shedding with nothing to join; Refused = the admission line.
        let spawn = || {
            warm::spawn_instance(
                Arc::clone(&prefix_data),
                req.limits.hard_memory_mb,
                caller.brand_key.clone(),
            )
        };
        let caps = warm::HeapCaps {
            soft_bytes: u64::from(req.limits.soft_memory_mb) * 1024 * 1024,
            hard_bytes: u64::from(req.limits.hard_memory_mb) * 1024 * 1024,
        };
        enum Taken {
            Pooled(warm::AttachedInstance),
            Cold(warm::InstanceHandle),
        }
        let taken = match self.warm.acquire(&req.prefix_id, caps, &spawn) {
            warm::Acquired::Attached(att) => Taken::Pooled(att),
            warm::Acquired::Cold(handle) => Taken::Cold(handle),
            warm::Acquired::Refused(refusal) => return Err(Refusal::Capacity(refusal)),
            warm::Acquired::SpawnFailed(e) => {
                crate::log!("[iso4-v8] PrefixRun {run_id} — failed to spawn instance thread: {e}");
                return Err(Refusal::Internal(format!(
                    "failed to spawn instance thread: {e}"
                )));
            }
        };
        let msgs_tx = match &taken {
            Taken::Pooled(att) => att.sender(),
            Taken::Cold(handle) => handle.sender(),
        };

        // The job goes out below on this same channel before the frontend
        // can deliver anything: FIFO puts the job first.
        register(RunHandle {
            token,
            run_id,
            events: msgs_tx.clone(),
            ctl: Arc::clone(&ctl),
        });

        // Runs on the instance thread before the completion goes out, so the
        // host's next run on this prefix finds the instance back in the
        // pool. An instance of a since-disposed prefix is dropped, not
        // pooled (lock order prefixes → warm).
        let release: sandbox::ReleaseHook = {
            let engine = Arc::clone(self);
            let prefix_id = req.prefix_id.clone();
            Box::new(move |outcome: sandbox::CallOutcome| {
                // A deterministic cold-start failure marks the prefix broken;
                // a prefix disposed meanwhile stays gone.
                if outcome.prefix_failed {
                    if let Err(failure) = &outcome.result {
                        let mut store = engine.prefixes.lock().unwrap_or_else(|p| p.into_inner());
                        if let Some(entry @ PrefixEntry::Ready(_)) = store.get_mut(&prefix_id) {
                            // Stored for the prefix's lifetime: clamp like the
                            // wire does and drop the fields blob it never carries.
                            let raw = failure.error.to_payload();
                            let cause = types::RunErrorPayload {
                                message: errors::clamp_error_text(&raw.message).into_owned(),
                                stack: raw
                                    .stack
                                    .as_deref()
                                    .map(|s| errors::clamp_error_text(s).into_owned()),
                                fields: None,
                                ..raw
                            };
                            crate::log!(
                                "[iso4-v8] prefix_id={prefix_id} marked broken: cold start \
                                 failed with {}: {}",
                                cause.code, cause.message
                            );
                            *entry = PrefixEntry::Broken(Arc::new(cause));
                            // Healthy instances of a broken prefix never serve
                            // again; drop them like a dispose does.
                            engine.warm.dispose_prefix(&prefix_id);
                        }
                    }
                }
                let (wall_time_ms, cpu_time_ms) = match &outcome.result {
                    Ok(output) => (output.wall_time_ms, output.cpu_time_ms),
                    Err(failure) => (failure.wall_time_ms, failure.cpu_time_ms),
                };
                match taken {
                    Taken::Pooled(att) => {
                        let store = engine.prefixes.lock().unwrap_or_else(|p| p.into_inner());
                        let prefix_alive =
                            matches!(store.get(&prefix_id), Some(PrefixEntry::Ready(_)));
                        engine.warm.release(
                            &prefix_id,
                            att.id,
                            outcome.tainted,
                            outcome.heap_used_bytes,
                            wall_time_ms,
                            cpu_time_ms,
                            prefix_alive,
                        );
                    }
                    Taken::Cold(handle) => {
                        // Never pooled: dropping the handle ends the owner
                        // thread and its isolate; give back the one-off slot.
                        drop(handle);
                        engine
                            .warm
                            .release_oneoff(outcome.tainted, wall_time_ms, cpu_time_ms);
                    }
                }
                match &outcome.result {
                    Ok(output) => trace!(
                        "[iso4-v8] PrefixRun {} succeeded in {:.3}ms",
                        run_id,
                        output.duration_ms
                    ),
                    Err(failure) => trace!(
                        "[iso4-v8] PrefixRun {} failed in {:.3}ms: {:?}",
                        run_id,
                        failure.duration_ms,
                        failure.error
                    ),
                }
            })
        };

        let job = Box::new(sandbox::CallJob {
            token,
            limits: sandbox::Limits::from(&req.limits),
            globals: req.globals,
            io: sandbox::RunIo::Instance {
                link: Arc::clone(&caller.link),
            },
            observer: Arc::clone(&caller.observer),
            call_id_counter: Arc::clone(&caller.call_ids),
            call: req.call,
            epilogue: Some(sandbox::EpilogueSpec {
                run_id,
                report_heap: true,
            }),
            release: Some(release),
            ctl_slot: Some(ctl),
        });

        let msg = sandbox::RoutedEvent::new(token, sandbox::RunEvent::Job(job));
        if let Err(e) = msgs_tx.send(msg) {
            // The instance thread died before accepting the job: finish it
            // here, which releases and answers on the caller's link.
            crate::log!("[iso4-v8] PrefixRun {run_id} — instance thread dead before dispatch");
            if let sandbox::RunEvent::Job(mut job) = e.0.event {
                let link = job.io.link();
                sandbox::finish_job(&mut job, link, sandbox::dead_instance_outcome());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::link::{channel_link, Addr, NoopObserver};

    #[test]
    fn refuse_sends_one_bare_failure_with_the_refusals_code() {
        let engine = Engine::new(0, 0);
        let (link, host) = channel_link();
        let caller = Caller {
            link,
            observer: Arc::new(NoopObserver),
            call_ids: Arc::new(AtomicU32::new(0)),
            brand_key: String::new(),
        };
        engine.refuse(&caller, 7, Refusal::PrefixDisposed("p1".to_string()));
        let (to, msg) = host.try_recv().expect("one answer");
        assert_eq!(to, Addr::Run(7));
        match msg {
            Outbound::Result {
                completion: types::RunCompletion::Failure(failure),
            } => {
                assert_eq!(failure.error.code, "ERR_PREFIX_DISPOSED");
                assert!(failure.stdout.is_empty() && failure.bridge_calls.is_empty());
            }
            other => panic!("expected a failure Result, got {other:?}"),
        }
        assert!(host.try_recv().is_err(), "exactly one answer");
    }
}
