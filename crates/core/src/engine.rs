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
    /// No such prefix here: never prepared, disposed, or prepared with another hash.
    PrefixUnknown(String),
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
            Refusal::PrefixUnknown(prefix_id) => plain(
                "ERR_PREFIX_UNKNOWN",
                format!("prefix '{prefix_id}' is unknown to the runtime: never prepared, disposed, or prepared with other code"),
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
    /// The run's heap lines: the one-off's own, or its prefix's.
    pub heap: types::HeapLimits,
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

/// One stored prefix, as [`Engine::list_prefixes`] reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrefixInfo {
    pub id: String,
    pub hash: [u8; 16],
    pub heap: types::HeapLimits,
    pub broken: bool,
}

/// One per process. A prefix prepared through any caller is visible to all
/// of them, so the host pool can route any run to any connection. A caller
/// starts its runs from one thread.
pub struct Engine {
    prefixes: Mutex<HashMap<String, PrefixEntry>>,
    /// Numeric prefix aliases for the crash line; ids come from the host.
    next_alias: AtomicU64,
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
        let mut engine = Self::new(budget, line);
        if options.admission_line == AdmissionLine::Auto {
            engine.warm = engine.warm.with_derived_line();
        }
        Ok(engine)
    }

    /// For tests that start V8 lazily (`testing::engine`).
    pub(crate) fn new(warm_budget_bytes: u64, hard_line_bytes: u64) -> Self {
        Self {
            prefixes: Mutex::new(HashMap::new()),
            next_alias: AtomicU64::new(0),
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

    /// Store a prefix under the host's `prefix_id`. Nothing is evaluated: a
    /// broken prefix fails its first run instead. `heap` applies to every
    /// run; every run must carry `hash`. A held id with the same hash and
    /// heap is the same prefix; otherwise `PrefixIdTaken`.
    #[allow(clippy::too_many_arguments)] // the prefix's parts, as the wire carries them
    pub fn prepare(
        &self,
        prefix_id: String,
        code: String,
        filename: Option<String>,
        heap: types::HeapLimits,
        globals: Vec<types::HostGlobalDef>,
        imports: Vec<types::ImportBinding>,
        hash: [u8; 16],
    ) -> Result<(), sandbox::RunError> {
        // Only bridge stubs are rebindable per run; value globals and shim
        // wrappers are replayed from the stored defs.
        let declared_globals: Vec<String> = globals
            .iter()
            .filter_map(|g| g.bridge_stub_name().map(str::to_string))
            .collect();
        let mut store = self.prefixes.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(held) = store.get(&prefix_id) {
            let (held_hash, held_heap, broken) = match held {
                PrefixEntry::Ready(data) => (data.hash, data.heap, false),
                PrefixEntry::Broken { hash, heap, .. } => (*hash, *heap, true),
            };
            let taken = if held_hash != hash {
                Some(format!("prefix id \"{prefix_id}\" is held with other code"))
            } else if held_heap != heap {
                let limits = |h: types::HeapLimits| {
                    let mut s = format!("memoryMb {}", h.soft_memory_mb);
                    if held_heap.hard_memory_mb != heap.hard_memory_mb {
                        s += &format!(", hardMemoryMb {}", h.hard_memory_mb);
                    }
                    s
                };
                Some(format!(
                    "prefix id \"{prefix_id}\" is held with other memory limits (held: {}, asked: {})",
                    limits(held_heap),
                    limits(heap),
                ))
            } else {
                None
            };
            match taken {
                Some(message) => {
                    drop(store);
                    crate::log!(
                        "[iso4-v8] Precompile — ERR_PREFIX_ID_TAKEN: prefix_id={prefix_id:?}"
                    );
                    return Err(sandbox::RunError::PrefixIdTaken(message));
                }
                None if !broken => {
                    drop(store);
                    crate::log!("[iso4-v8] prefix already held — prefix_id={prefix_id:?}");
                    return Ok(());
                }
                // A broken prefix with the same code gets a fresh attempt under a new alias.
                None => {}
            }
        }
        let alias = self.next_alias.fetch_add(1, Ordering::Relaxed);
        crate::log!(
            "[iso4-v8] prefix stored — prefix_id={prefix_id:?} alias={alias} \
             ({} source bytes, {} declared globals, {} declared imports)",
            code.len(),
            declared_globals.len(),
            imports.len(),
        );
        store.insert(
            prefix_id,
            PrefixEntry::Ready(Arc::new(PrefixData {
                id: alias,
                code,
                filename,
                hash,
                heap,
                globals,
                declared_globals,
                declared_imports: imports,
            })),
        );
        Ok(())
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
        crate::log!("[iso4-v8] DisposePrefix prefix_id={prefix_id:?} (existed={existed})");
    }

    pub fn stats(&self) -> warm::RegistryStats {
        self.warm.stats()
    }

    /// Every stored prefix, sorted by id: a copy taken under the store lock.
    pub fn list_prefixes(&self) -> Vec<PrefixInfo> {
        let mut list: Vec<PrefixInfo> = {
            let store = self.prefixes.lock().unwrap_or_else(|p| p.into_inner());
            store
                .iter()
                .map(|(id, entry)| {
                    let (hash, heap, broken) = match entry {
                        PrefixEntry::Ready(data) => (data.hash, data.heap, false),
                        PrefixEntry::Broken { hash, heap, .. } => (*hash, *heap, true),
                    };
                    PrefixInfo {
                        id: id.clone(),
                        hash,
                        heap,
                        broken,
                    }
                })
                .collect()
        };
        list.sort_unstable_by(|a, b| a.id.cmp(&b.id));
        list
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
        let run_cap_bytes = u64::from(req.heap.hard_memory_mb) * 1024 * 1024;
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
            heap: req.heap,
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
                    sandbox::Limits::new(&req.limits, req.heap),
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
            .and_then(|entry| match entry {
                PrefixEntry::Ready(data) => Some(Ok(Arc::clone(data))),
                PrefixEntry::Broken { hash, cause, .. } if *hash == req.hash => {
                    Some(Err(Arc::clone(cause)))
                }
                PrefixEntry::Broken { .. } => None,
            });
        let prefix_data =
            match entry {
                Some(Ok(data)) if data.hash == req.hash => data,
                Some(Err(cause)) => {
                    crate::log!(
                    "[iso4-v8] PrefixRun {run_id} — ERR_PREFIX_BROKEN: prefix_id={:?} cold start \
                     failed with {}: {}",
                    req.prefix_id, cause.code, cause.message
                );
                    return Err(Refusal::PrefixBroken {
                        prefix_id: req.prefix_id,
                        cause,
                    });
                }
                Some(Ok(_)) | None => {
                    crate::log!(
                        "[iso4-v8] PrefixRun {} — prefix_id={:?} not found with this hash",
                        run_id,
                        req.prefix_id
                    );
                    return Err(Refusal::PrefixUnknown(req.prefix_id));
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
                payload.code,
                payload.message
            );
            return Err(Refusal::Binding(error));
        }

        trace!(
            "[iso4-v8] PrefixRun {} (prefix_id={:?}, {} globals, {} imports, call={})",
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
                prefix_data.heap.hard_memory_mb,
                caller.brand_key.clone(),
            )
        };
        let caps = warm::HeapCaps {
            soft_bytes: u64::from(prefix_data.heap.soft_memory_mb) * 1024 * 1024,
            hard_bytes: u64::from(prefix_data.heap.hard_memory_mb) * 1024 * 1024,
        };
        enum Taken {
            Pooled(warm::AttachedInstance),
            Cold(warm::InstanceHandle),
        }
        let taken = match self
            .warm
            .acquire(&req.prefix_id, prefix_data.id, caps, &spawn)
        {
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
            heap: prefix_data.heap,
        });

        // Runs on the instance thread before the completion goes out, so the
        // host's next run on this prefix finds the instance back in the
        // pool. An instance of a since-disposed prefix is dropped, not
        // pooled (lock order prefixes → warm).
        let release: sandbox::ReleaseHook = {
            let engine = Arc::clone(self);
            let prefix_id = req.prefix_id.clone();
            // A reused id may hold a newer prefix by now: act only on this one.
            let alias = prefix_data.id;
            Box::new(move |outcome: sandbox::CallOutcome| {
                // A deterministic cold-start failure marks the prefix broken;
                // a prefix disposed meanwhile stays gone.
                if outcome.prefix_failed {
                    if let Err(failure) = &outcome.result {
                        let mut store = engine.prefixes.lock().unwrap_or_else(|p| p.into_inner());
                        if let Some(PrefixEntry::Ready(data)) =
                            store.get(&prefix_id).filter(|e| is_alias(e, alias))
                        {
                            let (hash, heap) = (data.hash, data.heap);
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
                                "[iso4-v8] prefix_id={prefix_id:?} marked broken: cold start \
                                 failed with {}: {}",
                                cause.code,
                                cause.message
                            );
                            store.insert(
                                prefix_id.clone(),
                                PrefixEntry::Broken {
                                    hash,
                                    heap,
                                    cause: Arc::new(cause),
                                },
                            );
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
                            store.get(&prefix_id).is_some_and(|e| is_alias(e, alias));
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
            limits: sandbox::Limits::new(&req.limits, prefix_data.heap),
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

/// The store entry is the live prefix with this alias.
fn is_alias(entry: &PrefixEntry, alias: u64) -> bool {
    matches!(entry, PrefixEntry::Ready(data) if data.id == alias)
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
        engine.refuse(&caller, 7, Refusal::PrefixUnknown("p1".to_string()));
        let (to, msg) = host.try_recv().expect("one answer");
        assert_eq!(to, Addr::Run(7));
        match msg {
            Outbound::Result {
                completion: types::RunCompletion::Failure(failure),
            } => {
                assert_eq!(failure.error.code, "ERR_PREFIX_UNKNOWN");
                assert!(failure.stdout.is_empty() && failure.bridge_calls.is_empty());
            }
            other => panic!("expected a failure Result, got {other:?}"),
        }
        assert!(host.try_recv().is_err(), "exactly one answer");
    }

    #[test]
    fn a_prefix_run_uses_the_cap_set_at_prepare() {
        crate::v8::init_platform();
        let engine = Arc::new(Engine::new(0, 0));
        let (link, host) = channel_link();
        let caller = Caller {
            link,
            observer: Arc::new(NoopObserver),
            call_ids: Arc::new(AtomicU32::new(0)),
            brand_key: String::new(),
        };
        let heap = types::HeapLimits {
            soft_memory_mb: 0,
            hard_memory_mb: 32,
        };
        // `grow` keeps ~64 MB: fits the default cap, not this one.
        let prefix_id = "grow".to_string();
        engine.prepare(
            prefix_id.clone(),
            "export function grow() { const a = []; for (let i = 0; i < 64; i++) a.push(new Array(1 << 17).fill(i)); return a.length }"
                .to_string(),
            None,
            heap,
            Vec::new(),
            Vec::new(),
            [0; 16],
        )
        .expect("stored");
        let req = types::PrefixRunRequest {
            run_id: 1,
            prefix_id,
            hash: [0; 16],
            limits: types::RunLimits::default(),
            globals: Vec::new(),
            import_rebinds: Vec::new(),
            call: types::CallSpec {
                export_path: "grow".to_string(),
                args_blob: crate::testval::to_blob(&crate::testval::TestValue::Array(Vec::new())),
            },
        };
        let mut handle = None;
        engine
            .prefix_run(req, &caller, &mut |h| handle = Some(h))
            .expect("prefix_run refused");
        assert_eq!(handle.expect("registered").heap, heap);
        let (_, msg) = host
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("a result");
        match msg {
            Outbound::Result {
                completion: types::RunCompletion::Failure(failure),
            } => assert_eq!(failure.error.code, "ERR_MEMORY_LIMIT"),
            other => panic!("expected ERR_MEMORY_LIMIT, got {other:?}"),
        }
    }

    #[test]
    fn a_prefix_run_with_another_hash_or_id_is_unknown() {
        let engine = Arc::new(Engine::new(0, 0));
        let (link, _host) = channel_link();
        let caller = Caller {
            link,
            observer: Arc::new(NoopObserver),
            call_ids: Arc::new(AtomicU32::new(0)),
            brand_key: String::new(),
        };
        let prefix_id = "f".to_string();
        engine
            .prepare(
                prefix_id.clone(),
                "export const f = () => 1".to_string(),
                None,
                types::HeapLimits::default(),
                Vec::new(),
                Vec::new(),
                [1; 16],
            )
            .expect("stored");
        for (prefix_id, hash) in [(prefix_id, [2; 16]), ("nope".to_string(), [1; 16])] {
            let req = types::PrefixRunRequest {
                run_id: 1,
                prefix_id: prefix_id.clone(),
                hash,
                limits: types::RunLimits::default(),
                globals: Vec::new(),
                import_rebinds: Vec::new(),
                call: types::CallSpec {
                    export_path: "f".to_string(),
                    args_blob: Vec::new(),
                },
            };
            match engine.prefix_run(req, &caller, &mut |_| panic!("must not register")) {
                Err(Refusal::PrefixUnknown(id)) => assert_eq!(id, prefix_id),
                other => panic!("expected PrefixUnknown, got {other:?}"),
            }
        }
    }

    #[test]
    fn a_broken_prefix_with_another_hash_is_unknown() {
        let engine = Arc::new(Engine::new(0, 0));
        let (link, _host) = channel_link();
        let caller = Caller {
            link,
            observer: Arc::new(NoopObserver),
            call_ids: Arc::new(AtomicU32::new(0)),
            brand_key: String::new(),
        };
        let cause = Refusal::Internal("cold start".to_string()).to_payload();
        engine.prefixes.lock().unwrap().insert(
            "p".to_string(),
            PrefixEntry::Broken {
                hash: [1; 16],
                heap: types::HeapLimits::default(),
                cause: Arc::new(cause),
            },
        );
        let req = |hash| types::PrefixRunRequest {
            run_id: 1,
            prefix_id: "p".to_string(),
            hash,
            limits: types::RunLimits::default(),
            globals: Vec::new(),
            import_rebinds: Vec::new(),
            call: types::CallSpec {
                export_path: "f".to_string(),
                args_blob: Vec::new(),
            },
        };
        let mut no_register = |_| panic!("must not register");
        match engine.prefix_run(req([2; 16]), &caller, &mut no_register) {
            Err(Refusal::PrefixUnknown(id)) => assert_eq!(id, "p"),
            other => panic!("expected PrefixUnknown, got {other:?}"),
        }
        match engine.prefix_run(req([1; 16]), &caller, &mut no_register) {
            Err(Refusal::PrefixBroken { prefix_id, .. }) => assert_eq!(prefix_id, "p"),
            other => panic!("expected PrefixBroken, got {other:?}"),
        }
    }

    fn store(
        engine: &Engine,
        id: &str,
        code: &str,
        heap: types::HeapLimits,
        hash: [u8; 16],
    ) -> Result<(), sandbox::RunError> {
        engine.prepare(
            id.to_string(),
            code.to_string(),
            None,
            heap,
            Vec::new(),
            Vec::new(),
            hash,
        )
    }

    fn alias_of(engine: &Engine, id: &str) -> u64 {
        match engine.prefixes.lock().unwrap().get(id) {
            Some(PrefixEntry::Ready(data)) => data.id,
            _ => panic!("no ready prefix {id}"),
        }
    }

    #[test]
    fn a_held_id_with_the_same_hash_and_heap_is_the_same_prefix() {
        let engine = Engine::new(0, 0);
        let heap = types::HeapLimits::default();
        store(&engine, "p", "export const a = 1", heap, [1; 16]).unwrap();
        let alias = alias_of(&engine, "p");
        store(&engine, "p", "export const a = 1", heap, [1; 16]).unwrap();
        assert_eq!(alias_of(&engine, "p"), alias, "nothing stored again");
    }

    #[test]
    fn a_held_id_with_other_code_or_heap_is_taken() {
        let engine = Engine::new(0, 0);
        let heap = |soft, hard| types::HeapLimits {
            soft_memory_mb: soft,
            hard_memory_mb: hard,
        };
        store(&engine, "p", "export const a = 1", heap(64, 87), [1; 16]).unwrap();
        let taken = |result: Result<(), sandbox::RunError>| match result {
            Err(e) => {
                assert_eq!(e.code(), "ERR_PREFIX_ID_TAKEN");
                e.message()
            }
            Ok(()) => panic!("expected ERR_PREFIX_ID_TAKEN"),
        };
        assert_eq!(
            taken(store(
                &engine,
                "p",
                "export const a = 2",
                heap(64, 87),
                [2; 16]
            )),
            "prefix id \"p\" is held with other code"
        );
        assert_eq!(
            taken(store(&engine, "p", "export const a = 1", heap(128, 87), [1; 16])),
            "prefix id \"p\" is held with other memory limits (held: memoryMb 64, asked: memoryMb 128)"
        );
        assert_eq!(
            taken(store(&engine, "p", "export const a = 1", heap(128, 160), [1; 16])),
            "prefix id \"p\" is held with other memory limits (held: memoryMb 64, hardMemoryMb 87, asked: memoryMb 128, hardMemoryMb 160)"
        );
        assert_eq!(alias_of(&engine, "p"), 0, "the held prefix stays");
    }

    #[test]
    fn a_disposed_id_takes_other_code() {
        let engine = Engine::new(0, 0);
        let heap = types::HeapLimits::default();
        store(&engine, "p", "export const a = 1", heap, [1; 16]).unwrap();
        engine.dispose_prefix("p");
        store(&engine, "p", "export const a = 2", heap, [2; 16]).unwrap();
        assert_eq!(alias_of(&engine, "p"), 1, "a new prefix under the old id");
    }

    #[test]
    fn a_broken_id_with_the_same_code_and_heap_is_rebuilt() {
        let engine = Engine::new(0, 0);
        let heap = types::HeapLimits::default();
        let other_heap = types::HeapLimits {
            soft_memory_mb: 64,
            hard_memory_mb: 87,
        };
        let cause = Refusal::Internal("cold start".to_string()).to_payload();
        let broken = || PrefixEntry::Broken {
            hash: [1; 16],
            heap,
            cause: Arc::new(cause.clone()),
        };
        engine
            .prefixes
            .lock()
            .unwrap()
            .insert("p".to_string(), broken());
        // Other code or other heap lines stay taken, broken or not.
        let other = store(&engine, "p", "export const a = 2", heap, [2; 16]);
        assert!(matches!(other, Err(sandbox::RunError::PrefixIdTaken(_))));
        let other = store(&engine, "p", "export const a = 1", other_heap, [1; 16]);
        assert!(matches!(other, Err(sandbox::RunError::PrefixIdTaken(_))));
        assert!(matches!(
            engine.prefixes.lock().unwrap().get("p"),
            Some(PrefixEntry::Broken { .. })
        ));
        store(&engine, "p", "export const a = 1", heap, [1; 16]).unwrap();
        let alias = alias_of(&engine, "p");
        store(&engine, "p", "export const a = 1", heap, [1; 16]).unwrap();
        assert_eq!(
            alias_of(&engine, "p"),
            alias,
            "a ready prefix is not rebuilt"
        );
    }

    #[test]
    fn list_prefixes_reports_the_store_sorted_by_id() {
        let engine = Engine::new(0, 0);
        assert!(engine.list_prefixes().is_empty());

        let heap = |soft, hard| types::HeapLimits {
            soft_memory_mb: soft,
            hard_memory_mb: hard,
        };
        store(&engine, "b", "export const b = 1", heap(64, 87), [2; 16]).unwrap();
        store(&engine, "a", "export const a = 1", heap(0, 32), [1; 16]).unwrap();
        let cause = Refusal::Internal("cold start".to_string()).to_payload();
        engine.prefixes.lock().unwrap().insert(
            "c".to_string(),
            PrefixEntry::Broken {
                hash: [3; 16],
                heap: heap(128, 160),
                cause: Arc::new(cause),
            },
        );
        let info = |id: &str, hash, heap, broken| PrefixInfo {
            id: id.to_string(),
            hash,
            heap,
            broken,
        };
        assert_eq!(
            engine.list_prefixes(),
            [
                info("a", [1; 16], heap(0, 32), false),
                info("b", [2; 16], heap(64, 87), false),
                info("c", [3; 16], heap(128, 160), true),
            ]
        );

        engine.dispose_prefix("b");
        let ids: Vec<String> = engine.list_prefixes().into_iter().map(|p| p.id).collect();
        assert_eq!(ids, ["a", "c"]);
    }
}
