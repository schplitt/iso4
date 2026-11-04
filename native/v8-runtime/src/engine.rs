//! The engine as a frontend sees it: one [`Engine`] per process owns the
//! prefix store and the warm registry. It answers through the caller's
//! [`HostLink`] and never sees a frame.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use crate::errors;
use crate::link::{HostLink, Inbound, Observer};
use crate::prefix::{validate_global_rebinds, validate_import_rebinds, PrefixData, PrefixEntry};
use crate::types;
use crate::v8 as sandbox;
use crate::warm;

/// Per-run trace lines (`Run`/`PrefixRun` received / succeeded / failed) are
/// off by default: two unbuffered `eprintln!`s cost 2-4 % of a hot
/// `prefix.call()`. `ISO4_V8_TRACE=1` turns them on.
pub(crate) fn trace_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        matches!(
            std::env::var("ISO4_V8_TRACE").as_deref(),
            Ok("1") | Ok("true")
        )
    })
}

/// `eprintln!` gated on [`trace_enabled`]; arguments are only evaluated when
/// tracing is on.
macro_rules! trace {
    ($($arg:tt)*) => {
        if crate::engine::trace_enabled() {
            eprintln!($($arg)*);
        }
    };
}
pub(crate) use trace;

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
    /// The runtime could not start the run.
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
                    eprintln!(
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

/// One per process. A prefix prepared through any caller is visible to all
/// of them, so the host pool can route any run to any connection. A caller
/// starts its runs from one thread.
pub struct Engine {
    prefixes: Mutex<HashMap<String, PrefixEntry>>,
    next_prefix_id: AtomicU64,
    /// Every prefix run is served by a resident instance from here; one-off
    /// runs share its slot accounting.
    warm: warm::WarmRegistry,
}

impl Engine {
    /// `warm_budget_bytes` is the mark the registry sheds against (global
    /// container usage; 0 disables it); `hard_line_bytes` is the isolate
    /// admission line (0 = no container limit readable).
    pub fn new(warm_budget_bytes: u64, hard_line_bytes: u64) -> Self {
        Self {
            prefixes: Mutex::new(HashMap::new()),
            next_prefix_id: AtomicU64::new(0),
            warm: warm::WarmRegistry::new(warm_budget_bytes, hard_line_bytes),
        }
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
        eprintln!(
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
        eprintln!("[iso4-v8] DisposePrefix prefix_id={prefix_id} (existed={existed})");
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
                crate::webcodec::set_session_brand_key(brand_key);
                sandbox::set_oom_run(sandbox::OomRun::Run(run_id));
                sandbox::set_run_epilogue_spec(Some(sandbox::EpilogueSpec {
                    run_id,
                    report_heap: false,
                }));
                let result = sandbox::execute_with_io(
                    &req.code,
                    req.filename.as_deref(),
                    sandbox::Limits::from(&req.limits),
                    &req.globals,
                    &req.imports,
                    sandbox::RunIo::Session {
                        events,
                        link: Arc::clone(&link),
                    },
                    token,
                    call_ids,
                    req.call.as_ref(),
                    Some(ctl),
                    observer,
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
            eprintln!("[iso4-v8] failed to spawn one-off worker: {e}");
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
                eprintln!(
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
                eprintln!(
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
            eprintln!(
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
                eprintln!("[iso4-v8] PrefixRun {run_id} — failed to spawn instance thread: {e}");
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
                            eprintln!(
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
            eprintln!("[iso4-v8] PrefixRun {run_id} — instance thread dead before dispatch");
            if let sandbox::RunEvent::Job(mut job) = e.0.event {
                let link = job.io.link();
                sandbox::finish_job(&mut job, link, warm::dead_instance_outcome());
            }
        }
        Ok(())
    }
}
