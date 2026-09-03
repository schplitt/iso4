//! Warm instance registry: prefix id → pool of resident isolates.
//!
//! Every prepared prefix is served by warm instances. An instance is one
//! isolate with the prefix already evaluated, owned cradle-to-grave by a
//! dedicated OS thread — rusty_v8 pins isolates to their creating thread
//! (`OwnedIsolate` is `!Send`, `Drop` asserts current-thread ownership, no
//! `Locker` is exposed), so the owner thread is the only place the isolate
//! can be created, called into, and dropped. The owner thread runs the
//! per-instance turn loop (`v8::serve_instance`): jobs arrive over the
//! handle's job channel, inbound frames arrive token-tagged on the handle's
//! ONE event channel (demux-routed), and any number of session runs can be
//! suspended on one instance at once. Session
//! dispatch never blocks — a job's completion hook writes the run's frames
//! through the connection's serialized writer and releases the instance;
//! only the direct API (`InstanceHandle::call`, tests) waits on a respond
//! channel.
//!
//! Capacity model (v4, #77 — full rationale: DESIGN.md §13.2.1): metered
//! against GLOBAL container memory (`container.rs`; child RSS where no
//! cgroup exists). Two lines: the warm budget latches shedding (evict idle
//! by `heapUsed × idleTime`, stop pooling new; release at 4/5), and the
//! hard admission line (90% of limit − reserve) refuses CREATING an
//! isolate that could tip the container — refused runs fail, never queue;
//! uncapped runs are refused from the budget mark. Reuse is never gated.
//! Every acquire/release samples usage and folds it through `policy.rs`.
//! Running instances are never evicted; no instance-count cap exists.
//!
//! Instances of one prefix share no state with each other; state inside one
//! instance survives between calls as a cache, never a guarantee — any
//! instance may be evicted at any moment (taint, scored eviction, dispose).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::policy;
use crate::rss;
use crate::session::PrefixData;
use crate::v8 as sandbox;

/// One call forwarded to an instance owner thread — see [`sandbox::CallJob`]
/// (the job type lives beside the turn loop that executes it).
pub use crate::v8::CallJob;

/// A live warm instance as the registry sees it: the channel to its owner
/// thread plus the metadata eviction needs. Dropping the handle disconnects
/// the channel; the owner thread then exits (once its in-flight runs drain)
/// and disposes the isolate on the thread that created it.
pub struct InstanceHandle {
    jobs: crossbeam_channel::Sender<sandbox::JobMsg>,
    /// The instance's ONE run-event channel: the session demux tags every
    /// routed event with the owning run's table token, and the owner loop
    /// delivers by token — no per-run channel, no per-event select rebuild.
    events: sandbox::RunEventSender,
    /// When this instance last finished a call — the idleTime factor of the
    /// eviction score.
    last_used: Instant,
    /// `used_heap_size` after the last call — Result-frame report and the
    /// heap factor of the eviction score.
    pub heap_used_bytes: u64,
    /// The owner loop's thread-load signal — join routing's saturation
    /// input.
    load: Arc<sandbox::InstanceLoad>,
}

/// What a run holds while attached to a pooled instance: the channels plus
/// the registry id `release` needs. The registry keeps the owning
/// [`InstanceHandle`]; several runs may hold attachments to one instance at
/// once (the loop interleaves them).
pub struct AttachedInstance {
    pub id: u64,
    jobs: crossbeam_channel::Sender<sandbox::JobMsg>,
    events: sandbox::RunEventSender,
}

impl AttachedInstance {
    pub fn sender(&self) -> crossbeam_channel::Sender<sandbox::JobMsg> {
        self.jobs.clone()
    }

    pub fn event_sender(&self) -> sandbox::RunEventSender {
        self.events.clone()
    }
}

impl InstanceHandle {
    /// Run one call on this instance and wait for the outcome. A dead owner
    /// thread (panic during the call) reports an internal failure with
    /// `tainted: true`, so the caller evicts it like any other taint.
    pub fn call(&self, job: CallJob) -> sandbox::CallOutcome {
        let (tx, rx) = crossbeam_channel::bounded(1);
        if self.jobs.send((Box::new(job), Some(tx))).is_err() {
            return dead_instance_outcome();
        }
        rx.recv().unwrap_or_else(|_| dead_instance_outcome())
    }

    /// A clone of the job channel's sender, for the session dispatch path
    /// where the handle itself moves into the job's completion hook before
    /// the job is sent.
    pub fn sender(&self) -> crossbeam_channel::Sender<sandbox::JobMsg> {
        self.jobs.clone()
    }

    /// A clone of the instance's run-event sender, for the demux route of a
    /// run dispatched to this instance (tag every event with the run's
    /// token).
    pub fn event_sender(&self) -> sandbox::RunEventSender {
        self.events.clone()
    }
}

pub(crate) fn dead_instance_outcome() -> sandbox::CallOutcome {
    sandbox::CallOutcome {
        result: Err(sandbox::FailureOutput {
            error: sandbox::RunError::Internal(
                "warm instance thread died before completing the call".to_string(),
            ),
            stdout: Vec::new(),
            stderr: Vec::new(),
            duration_ms: 0.0,
            cpu_time_ms: 0.0,
            bridge_calls: Vec::new(),
        }),
        tainted: true,
        heap_used_bytes: 0,
    }
}

/// Spawn the owner thread for one instance of `prefix`. The isolate is
/// created lazily on the first job so a creation failure surfaces as that
/// call's result (warm-up still runs under its own fixed budget — the
/// failure is reported to, not billed to, the triggering call).
///
/// Thread spawn itself can fail (process resource exhaustion); that error
/// is the caller's to answer — it runs on the session demux, where a panic
/// would cost the whole connection instead of the one run.
///
/// `brand_key` is the spawning session's descriptor brand key; it is per
/// sandbox (every connection of one host sends the same token), so installing
/// the spawner's key covers every later call routed to this instance.
pub fn spawn_instance(
    prefix: Arc<PrefixData>,
    memory_mb: u32,
    brand_key: String,
) -> std::io::Result<InstanceHandle> {
    let (tx, rx) = crossbeam_channel::unbounded::<sandbox::JobMsg>();
    let (etx, erx) = crossbeam_channel::unbounded::<sandbox::RoutedEvent>();
    let load = Arc::new(sandbox::InstanceLoad::new());
    let thread_load = Arc::clone(&load);
    std::thread::Builder::new()
        .name("iso4-warm-instance".to_string())
        .spawn(move || instance_main(prefix, memory_mb, brand_key, rx, erx, thread_load))?;
    Ok(InstanceHandle {
        jobs: tx,
        events: etx,
        last_used: Instant::now(),
        heap_used_bytes: 0,
        load,
    })
}

fn instance_main(
    prefix: Arc<PrefixData>,
    memory_mb: u32,
    brand_key: String,
    jobs: crossbeam_channel::Receiver<sandbox::JobMsg>,
    events: crossbeam_channel::Receiver<sandbox::RoutedEvent>,
    load: Arc<sandbox::InstanceLoad>,
) {
    // Host-type descriptors deserialized on this thread — call args, bridge
    // responses, and the cold-start replay of stored data globals — rehydrate
    // only when stamped with this key.
    crate::webcodec::set_session_brand_key(brand_key);

    // The isolate is created lazily on the first job so a creation failure
    // surfaces as that call's result.
    let Ok((mut first_job, first_respond)) = jobs.recv() else {
        return; // evicted before ever serving
    };
    let spec = sandbox::PrefixSpec {
        code: &prefix.code,
        filename: prefix.filename.as_deref().unwrap_or("<prefix>"),
        globals: &prefix.globals,
    };
    let mut core = match sandbox::create_instance_core(
        Some(spec),
        &prefix.declared_imports,
        memory_mb,
    ) {
        Ok(c) => c,
        Err(f) => {
            // Warm-up failed — report it as this call's outcome and exit;
            // the registry drops the handle on taint.
            let outcome = sandbox::CallOutcome {
                result: Err(f),
                tainted: true,
                heap_used_bytes: 0,
            };
            if let Some(complete) = first_job.complete.take() {
                complete(outcome);
            } else if let Some(respond) = first_respond {
                respond.send(outcome).ok();
            }
            return;
        }
    };

    // The per-instance turn loop: jobs, routed frames, deadlines — any
    // number of session runs suspended at once (#125).
    sandbox::serve_instance(
        &mut core,
        &prefix.globals,
        &prefix.declared_imports,
        (first_job, first_respond),
        &jobs,
        &events,
        &load,
    );
    // The loop ended — the registry evicted this instance (taint, eviction,
    // dispose) or the process is shutting down. `core` drops here, on the
    // thread that created the isolate, as rusty_v8 requires.
}

/// What `acquire` hands the session thread.
pub enum Acquired {
    /// A pooled instance took the run — reused idle, joined busy, or
    /// freshly spawned, uniformly. Dispatch through it, then `release`
    /// with its id.
    Attached(AttachedInstance),
    /// Shedding with no live instance to join: fresh unpooled isolate for
    /// this call only. Run it, then DROP the handle and `release_oneoff`.
    Cold(InstanceHandle),
    /// The memory admission refuses a new isolate, nothing idle exists and
    /// nothing is joinable. Fail the run with `ERR_CAPACITY` carrying this
    /// message; no registry state was taken.
    Refused(String),
    /// The instance thread could not be spawned (process resource
    /// exhaustion). Fail the run as internal; no registry state was taken.
    SpawnFailed(std::io::Error),
}

/// A point-in-time snapshot of the registry for `stats()`. Counts are
/// consistent with each other (taken under one lock), stale the moment the
/// lock drops — diagnostics, not synchronization.
pub struct RegistryStats {
    /// Running one-off isolates (`sandbox.run()`).
    pub oneoff_running: usize,
    /// Warm instances currently serving at least one run.
    pub warm_busy: usize,
    /// Runs in flight on warm instances (several may share one instance).
    pub warm_runs: usize,
    /// Idle warm instances ready for reuse.
    pub warm_idle: usize,
    /// Summed `heap_used_bytes` of the idle instances (last-call
    /// measurements; busy instances' current heap is unknown mid-call).
    pub idle_heap_bytes: u64,
    /// Per-prefix `(prefix_id, idle, busy)` instance counts.
    pub per_prefix: Vec<(String, usize, usize)>,
    /// The warm budget this registry sheds against (0 = disabled) — next
    /// to `usage_bytes` so utilization is computable from one snapshot.
    pub warm_budget_bytes: u64,
    /// The runtime process's own RSS at snapshot time (0 when unreadable)
    /// — diagnostics for the child's share of the container.
    pub rss_bytes: u64,
    /// Measured global container usage at snapshot time — the signal both
    /// lines act on (cgroup working set; falls back to `rss_bytes` where
    /// no cgroup exists; 0 when neither is readable).
    pub usage_bytes: u64,
    /// The hard admission line (0 = no container limit readable).
    pub hard_line_bytes: u64,
    /// The shedding latch: usage reached the budget and has not yet fallen
    /// back to 4/5 of it.
    pub under_pressure: bool,
}

pub struct WarmRegistry {
    inner: Mutex<RegistryInner>,
    /// The warm budget in bytes (`--warm-budget-bytes`), the shedding mark.
    /// 0 disables the watermarks — nothing bounds warmth then except
    /// `dispose()`; the host's default budget makes that an explicit
    /// opt-out, not a default.
    warm_budget_bytes: u64,
    /// The hard admission line in bytes (90% of container limit − Node
    /// reserve, `container::admission_line_bytes()`). 0 = disabled (no
    /// container limit readable).
    hard_line_bytes: u64,
    /// Test seam: watermark tests inject a usage sample instead of reading
    /// the real meter (u64::MAX = unset). Never set outside tests.
    #[cfg(test)]
    usage_override: std::sync::atomic::AtomicU64,
}

/// A fading average: one number, old data decays out (α = 0.2).
#[derive(Default, Clone, Copy)]
struct Ewma {
    value: f64,
    seeded: bool,
}

impl Ewma {
    fn update(&mut self, sample: f64) {
        self.value = if self.seeded {
            self.value * 0.8 + sample * 0.2
        } else {
            sample
        };
        self.seeded = true;
    }

    fn get(&self) -> Option<f64> {
        self.seeded.then_some(self.value)
    }
}

/// Per-prefix demand statistics feeding the join/spawn decision. Lives on
/// the prefix entry — survives eviction, dies at dispose (#77 ruling).
#[derive(Default)]
struct PrefixDemand {
    last_arrival: Option<Instant>,
    /// Mean inter-arrival gap (ms); rate = 1000 / this.
    interval_ms: Ewma,
    /// Mean CPU per completed run (ms) — the spawn signal.
    cpu_ms: Ewma,
    /// Mean measured heap at release — the admission input for further
    /// instances of a recurring prefix (seeded with the run cap until real
    /// samples exist, #77 expansion item 4).
    heap_bytes: Ewma,
}

impl PrefixDemand {
    fn note_arrival(&mut self, now: Instant) {
        if let Some(last) = self.last_arrival {
            self.interval_ms
                .update((now - last).as_secs_f64() * 1000.0);
        }
        self.last_arrival = Some(now);
    }

    fn arrivals_per_sec(&self) -> f64 {
        match self.interval_ms.get() {
            Some(ms) if ms > 0.0 => 1000.0 / ms,
            _ => 0.0,
        }
    }
}

/// One pooled instance as the registry tracks it. `in_flight == 0` = idle
/// (evictable); `dead` = tainted or prefix-disposed — no new attachments,
/// the handle drops (owner thread exits) once the last run releases.
struct Instance {
    id: u64,
    handle: InstanceHandle,
    /// Runs currently attached (parked ones included).
    in_flight: usize,
    /// The heap cap this instance's isolate was created with — attach
    /// refuses a mismatching run (#81 r6).
    cap_bytes: u64,
    dead: bool,
}

/// Instances + demand stats of one prefix. The entry persists across
/// evictions (the stats must outlive the instances) and is removed at
/// dispose.
#[derive(Default)]
struct PrefixEntry {
    instances: Vec<Instance>,
    demand: PrefixDemand,
}

struct RegistryInner {
    /// Per-prefix state, removed only at `dispose_prefix` (stats survive
    /// eviction).
    prefixes: HashMap<String, PrefixEntry>,
    /// Live isolates: pooled warm + running one-off.
    total: usize,
    /// Running one-off isolates — stats only; they also count in `total`.
    oneoff_running: usize,
    /// Idle (in_flight == 0, non-dead) instances across all prefixes —
    /// maintained (not recomputed) so the per-event pressure check stays
    /// O(1). `stats()` cross-checks it against the recomputed sum.
    idle_total: usize,
    /// Monotonic instance-id source for `release` addressing.
    next_instance_id: u64,
    /// The shedding latch: fed back into the next watermark verdict as
    /// `was_shedding` (hysteresis).
    shedding: bool,
    /// The last completed shed pass — the futility check compares the next
    /// usage sample against it. Cleared when the latch releases.
    last_pass: Option<policy::PassOutcome>,
}

impl RegistryInner {
    /// Fold one RSS sample through the watermark policy and perform the
    /// verdict: update the latch, shed the pass's worth of victims, record
    /// the pass for the futility check. Returns whether a NEW instance may
    /// be pooled (`false` while shedding — celld couples the admission
    /// stop to the latch). `None` (no reading available) releases the
    /// latch and admits — watermarks unavailable is not pressure, and a
    /// latch held with no signal to ever release it would report
    /// `underPressure` forever while admission has in fact resumed.
    fn pressure_pass(&mut self, usage_sample: Option<u64>, budget_bytes: u64) -> bool {
        let Some(usage_bytes) = usage_sample else {
            self.shedding = false;
            self.last_pass = None;
            return true;
        };
        let verdict = policy::watermark_action(&policy::PressureFacts {
            usage_bytes,
            budget_bytes,
            was_shedding: self.shedding,
            last_pass: self.last_pass,
            idle_count: self.idle_total,
        });
        self.shedding = verdict.shedding;
        if !verdict.shedding {
            self.last_pass = None;
        }
        if verdict.evict > 0 {
            let now = Instant::now();
            let mut dropped = 0;
            while dropped < verdict.evict && evict_scored(self, now) {
                dropped += 1;
            }
            if dropped > 0 {
                self.last_pass = Some(policy::PassOutcome {
                    usage_at_pass: usage_bytes,
                });
            }
        }
        !verdict.shedding
    }
}

/// The per-prefix instance ceiling: more instances than cores can never
/// execute in parallel.
fn cores() -> usize {
    static CORES: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *CORES.get_or_init(|| std::thread::available_parallelism().map_or(1, |n| n.get()))
}

impl WarmRegistry {
    pub fn new(warm_budget_bytes: u64, hard_line_bytes: u64) -> Self {
        Self {
            inner: Mutex::new(RegistryInner {
                prefixes: HashMap::new(),
                total: 0,
                oneoff_running: 0,
                idle_total: 0,
                next_instance_id: 1,
                shedding: false,
                last_pass: None,
            }),
            warm_budget_bytes,
            hard_line_bytes,
            #[cfg(test)]
            usage_override: std::sync::atomic::AtomicU64::new(u64::MAX),
        }
    }

    /// One usage sample for the pressure/admission checks — taken BEFORE
    /// the registry lock (a read does not belong under the mutex): global
    /// container usage where a cgroup exists, the child's own RSS
    /// otherwise. Skipped entirely when both lines are disabled, so that
    /// path pays nothing.
    fn sample_usage(&self) -> Option<u64> {
        if self.warm_budget_bytes == 0 && self.hard_line_bytes == 0 {
            return None;
        }
        #[cfg(test)]
        {
            let injected = self.usage_override.load(std::sync::atomic::Ordering::Relaxed);
            if injected != u64::MAX {
                return Some(injected);
            }
        }
        crate::container::usage_bytes().or_else(rss::process_rss_bytes)
    }

    /// Watermark tests inject the usage sample; everything downstream of
    /// the number is deterministic (the point of the pure policy layer).
    #[cfg(test)]
    fn set_usage_for_test(&self, usage_bytes: u64) {
        self.usage_override
            .store(usage_bytes, std::sync::atomic::Ordering::Relaxed);
    }

    /// May a NEW isolate be created for a run capped at `run_cap_bytes`?
    /// `Err` carries the refusal message for the run's `ERR_CAPACITY`.
    fn admit(&self, usage: Option<u64>, run_cap_bytes: u64) -> Result<(), String> {
        // No reading = no lines (matches `pressure_pass`: unavailable
        // watermarks are not pressure).
        let Some(usage_bytes) = usage else {
            return Ok(());
        };
        let facts = policy::AdmitFacts {
            usage_bytes,
            run_cap_bytes,
            hard_line_bytes: self.hard_line_bytes,
            budget_bytes: self.warm_budget_bytes,
        };
        if policy::admit_isolate(&facts) {
            return Ok(());
        }
        const MB: u64 = 1024 * 1024;
        Err(if run_cap_bytes == 0 {
            format!(
                "no capacity for a new isolate: this run is uncapped (memoryMb: 0) and \
                 measured container memory ({} MB) is at or above the memory budget ({} MB)",
                usage_bytes / MB,
                self.warm_budget_bytes / MB,
            )
        } else {
            format!(
                "no capacity for a new isolate: measured container memory ({} MB) plus \
                 this run's memoryMb ({} MB) crosses the admission line ({} MB — 90% of \
                 the container limit minus the host reserve)",
                usage_bytes / MB,
                run_cap_bytes / MB,
                self.hard_line_bytes / MB,
            )
        })
    }

    /// Take an instance for a run of `prefix_id` capped at `run_cap_bytes`
    /// (0 = uncapped). Order: reuse the warmest idle instance; else decide
    /// join-or-spawn from the prefix's demand EWMAs (spawn on CPU demand,
    /// ceiling at cores; routing packs onto the fullest non-saturated
    /// instance, spilling over to a spawn and then to the least-utilized).
    /// Spawns are gated by the shedding latch and the admission line —
    /// joins never are (they add no memory). `spawn` runs under the
    /// registry lock: thread creation only, the isolate boots lazily on
    /// the owner thread.
    pub fn acquire(
        &self,
        prefix_id: &str,
        run_cap_bytes: u64,
        spawn: &dyn Fn() -> std::io::Result<InstanceHandle>,
    ) -> Acquired {
        let usage = self.sample_usage();
        let now = Instant::now();
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let inner = &mut *guard;
        let admit_warm = inner.pressure_pass(usage, self.warm_budget_bytes);
        let id = inner.next_instance_id;
        let entry = inner.prefixes.entry(prefix_id.to_string()).or_default();
        entry.demand.note_arrival(now);
        if !entry.demand.heap_bytes.seeded && run_cap_bytes > 0 {
            // Seed: the theoretical cap, until measurements fade it out.
            entry.demand.heap_bytes.update(run_cap_bytes as f64);
        }

        // Fast path: the warmest idle instance (cap must match, #81 r6).
        let idle_pos = entry
            .instances
            .iter()
            .enumerate()
            .filter(|(_, i)| !i.dead && i.in_flight == 0 && i.cap_bytes == run_cap_bytes)
            .max_by_key(|(_, i)| i.handle.last_used)
            .map(|(pos, _)| pos);
        if let Some(pos) = idle_pos {
            let inst = &mut entry.instances[pos];
            inst.in_flight = 1;
            inner.idle_total = inner.idle_total.saturating_sub(1);
            return Acquired::Attached(attachment(inst));
        }

        let joinable: Vec<usize> = entry
            .instances
            .iter()
            .enumerate()
            .filter(|(_, i)| !i.dead && i.cap_bytes == run_cap_bytes)
            .map(|(pos, _)| pos)
            .collect();
        let live = entry.instances.iter().filter(|i| !i.dead).count();

        if joinable.is_empty() {
            // First isolate of this prefix (or only dead/mismatched ones):
            // admission line, then latch, then spawn.
            if let Err(refusal) = self.admit(usage, run_cap_bytes) {
                return Acquired::Refused(refusal);
            }
            let handle = match spawn() {
                Ok(h) => h,
                Err(e) => return Acquired::SpawnFailed(e),
            };
            inner.total += 1;
            if !admit_warm {
                // Shedding: serve this call on an unpooled isolate.
                inner.oneoff_running += 1;
                return Acquired::Cold(handle);
            }
            inner.next_instance_id += 1;
            entry.instances.push(Instance {
                id,
                handle,
                in_flight: 1,
                cap_bytes: run_cap_bytes,
                dead: false,
            });
            return Acquired::Attached(attachment(entry.instances.last().unwrap()));
        }

        // Join or open another instance? Admission for a further instance
        // of a recurring prefix uses the measured heap average, not the
        // theoretical cap (#77 expansion item 4).
        let spawn_cap = entry
            .demand
            .heap_bytes
            .get()
            .map_or(run_cap_bytes, |h| h as u64);
        let can_spawn =
            admit_warm && live < cores() && self.admit(usage, spawn_cap).is_ok();
        let facts: Vec<policy::JoinFact> = joinable
            .iter()
            .map(|&pos| {
                let i = &entry.instances[pos];
                policy::JoinFact {
                    in_flight: i.in_flight,
                    util_permille: i.handle.load.current_util_permille(),
                }
            })
            .collect();
        let target = policy::pick_join_target(&facts);
        let wants_spawn = policy::should_spawn(&policy::SpawnFacts {
            arrivals_per_sec: entry.demand.arrivals_per_sec(),
            cpu_ms_per_run: entry.demand.cpu_ms.get(),
            live_instances: live,
            cores: cores(),
        });

        let join = if (wants_spawn || target.is_none()) && can_spawn {
            None // spawn: demanded, or spillover past saturated targets
        } else {
            target.or_else(|| policy::join_fallback(&facts))
        };
        match join {
            Some(fact_idx) => {
                let inst = &mut entry.instances[joinable[fact_idx]];
                inst.in_flight += 1;
                Acquired::Attached(attachment(inst))
            }
            None => {
                let handle = match spawn() {
                    Ok(h) => h,
                    Err(e) => return Acquired::SpawnFailed(e),
                };
                inner.total += 1;
                inner.next_instance_id += 1;
                entry.instances.push(Instance {
                    id,
                    handle,
                    in_flight: 1,
                    cap_bytes: run_cap_bytes,
                    dead: false,
                });
                Acquired::Attached(attachment(entry.instances.last().unwrap()))
            }
        }
    }

    /// Same ledger accounting for a one-off run (fresh isolate, never
    /// reused). Shedding never refuses it — transient work gives its
    /// memory back on its own (celld keeps stateless admission open under
    /// pressure for the same reason) — but the hard admission line does:
    /// a one-off is an isolate creation like any other. `Err` = refuse the
    /// run with `ERR_CAPACITY`; no counters were taken.
    pub fn reserve_oneoff(&self, run_cap_bytes: u64) -> Result<(), String> {
        let usage = self.sample_usage();
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        inner.pressure_pass(usage, self.warm_budget_bytes);
        self.admit(usage, run_cap_bytes)?;
        inner.total += 1;
        inner.oneoff_running += 1;
        Ok(())
    }

    /// A one-off run finished. Also the release path for `Cold` instances:
    /// the session drops the handle instead of pooling it, and the owner
    /// thread disposes the isolate asynchronously — the ledger decrements
    /// slightly ahead of the actual memory release, which the next usage
    /// sample absorbs.
    pub fn release_oneoff(&self) {
        let usage = self.sample_usage();
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        inner.total = inner.total.saturating_sub(1);
        inner.oneoff_running = inner.oneoff_running.saturating_sub(1);
        inner.pressure_pass(usage, self.warm_budget_bytes);
    }

    /// One run released its instance. The instance goes idle when its last
    /// run leaves; tainted instances and instances of a disposed prefix
    /// are marked dead — no new joins — and dropped once drained (the
    /// owner thread exits and disposes the isolate). Clean completions
    /// feed the prefix's demand EWMAs.
    #[allow(clippy::too_many_arguments)] // the release facts, all scalars
    pub fn release(
        &self,
        prefix_id: &str,
        instance_id: u64,
        outcome_tainted: bool,
        heap_used_bytes: u64,
        cpu_time_ms: f64,
        prefix_alive: bool,
    ) {
        let usage = self.sample_usage();
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let inner = &mut *guard;
        if let Some(entry) = inner.prefixes.get_mut(prefix_id) {
            if !outcome_tainted {
                entry.demand.cpu_ms.update(cpu_time_ms);
                if heap_used_bytes > 0 {
                    entry.demand.heap_bytes.update(heap_used_bytes as f64);
                }
            }
            if let Some(pos) = entry.instances.iter().position(|i| i.id == instance_id) {
                let inst = &mut entry.instances[pos];
                inst.in_flight = inst.in_flight.saturating_sub(1);
                if outcome_tainted || !prefix_alive {
                    inst.dead = true;
                }
                if inst.dead {
                    if inst.in_flight == 0 {
                        entry.instances.remove(pos);
                        inner.total = inner.total.saturating_sub(1);
                    }
                } else if inst.in_flight == 0 {
                    inst.handle.last_used = Instant::now();
                    inst.handle.heap_used_bytes = heap_used_bytes;
                    inner.idle_total += 1;
                }
            }
            if !prefix_alive && entry.instances.is_empty() {
                inner.prefixes.remove(prefix_id);
            }
        }
        // Pressure check AFTER pooling: the just-released instance is a
        // candidate like any other (the idleTime factor protects it).
        inner.pressure_pass(usage, self.warm_budget_bytes);
    }

    /// Mark every instance of a disposed prefix dead: idle ones drop now,
    /// busy ones once their runs release; the entry (stats included) goes
    /// with the last of them.
    pub fn dispose_prefix(&self, prefix_id: &str) {
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let inner = &mut *guard;
        let Some(entry) = inner.prefixes.get_mut(prefix_id) else {
            return;
        };
        let mut dropped = 0;
        entry.instances.retain_mut(|inst| {
            inst.dead = true;
            if inst.in_flight == 0 {
                dropped += 1;
                false // handle drops → owner thread exits
            } else {
                true
            }
        });
        inner.total = inner.total.saturating_sub(dropped);
        inner.idle_total = inner.idle_total.saturating_sub(dropped);
        if entry.instances.is_empty() {
            inner.prefixes.remove(prefix_id);
        }
    }

    /// Snapshot the registry for the `Stats` frame.
    pub fn stats(&self) -> RegistryStats {
        let inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let mut warm_busy = 0;
        let mut warm_runs = 0;
        let mut warm_idle = 0;
        let mut idle_heap_bytes = 0u64;
        let mut per_prefix: Vec<(String, usize, usize)> = inner
            .prefixes
            .iter()
            .filter(|(_, entry)| !entry.instances.is_empty())
            .map(|(id, entry)| {
                let busy = entry.instances.iter().filter(|i| i.in_flight > 0).count();
                let idle = entry
                    .instances
                    .iter()
                    .filter(|i| i.in_flight == 0 && !i.dead)
                    .count();
                warm_busy += busy;
                warm_idle += idle;
                warm_runs += entry.instances.iter().map(|i| i.in_flight).sum::<usize>();
                idle_heap_bytes += entry
                    .instances
                    .iter()
                    .filter(|i| i.in_flight == 0 && !i.dead)
                    .map(|i| i.handle.heap_used_bytes)
                    .sum::<u64>();
                (id.clone(), idle, busy)
            })
            .collect();
        // HashMap iteration order is arbitrary; stable output for tests/logs.
        per_prefix.sort_by(|a, b| a.0.cmp(&b.0));
        debug_assert_eq!(
            warm_idle, inner.idle_total,
            "maintained idle_total drifted from the recomputed sum"
        );
        RegistryStats {
            oneoff_running: inner.oneoff_running,
            warm_busy,
            warm_runs,
            warm_idle,
            idle_heap_bytes,
            per_prefix,
            warm_budget_bytes: self.warm_budget_bytes,
            // Read directly (not `sample_usage`): stats are diagnostics
            // and want the numbers even when both lines are disabled. The
            // test override still applies so watermark tests see the same
            // usage the verdicts acted on.
            rss_bytes: rss::process_rss_bytes().unwrap_or(0),
            usage_bytes: self.stats_usage_bytes(),
            hard_line_bytes: self.hard_line_bytes,
            under_pressure: inner.shedding,
        }
    }

    fn stats_usage_bytes(&self) -> u64 {
        #[cfg(test)]
        {
            let injected = self.usage_override.load(std::sync::atomic::Ordering::Relaxed);
            if injected != u64::MAX {
                return injected;
            }
        }
        crate::container::usage_bytes()
            .or_else(rss::process_rss_bytes)
            .unwrap_or(0)
    }
}

/// The lightweight face a run holds while attached.
fn attachment(inst: &Instance) -> AttachedInstance {
    AttachedInstance {
        id: inst.id,
        jobs: inst.handle.jobs.clone(),
        events: inst.handle.events.clone(),
    }
}

/// Drop the highest-scored idle instance across ALL prefixes:
/// `heapUsed × idleTime`, ties to the longest-idle — `policy::pick_victim`
/// decides, this function only gathers facts and performs. Returns false
/// when nothing is idle. The single victim-picking path: every eviction
/// (shed passes today, anything later) comes through here, so policies
/// can never disagree about what may be taken.
fn evict_scored(inner: &mut RegistryInner, now: Instant) -> bool {
    let mut facts: Vec<policy::VictimFact> = Vec::with_capacity(inner.idle_total);
    let mut locations: Vec<(&String, u64)> = Vec::with_capacity(inner.idle_total);
    for (prefix_id, entry) in &inner.prefixes {
        for inst in entry.instances.iter().filter(|i| i.in_flight == 0 && !i.dead) {
            facts.push(policy::VictimFact {
                heap_used_bytes: inst.handle.heap_used_bytes,
                last_used: inst.handle.last_used,
            });
            locations.push((prefix_id, inst.id));
        }
    }
    let Some(pick) = policy::pick_victim(&facts, now) else {
        return false;
    };
    let (prefix_id, instance_id) = (locations[pick].0.clone(), locations[pick].1);
    let entry = inner
        .prefixes
        .get_mut(&prefix_id)
        .expect("victim key came from the map");
    entry.instances.retain(|i| i.id != instance_id);
    // The entry stays even at zero instances: its demand stats outlive
    // eviction by design (they die at dispose).
    inner.total = inner.total.saturating_sub(1);
    inner.idle_total = inner.idle_total.saturating_sub(1);
    true
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testval::{self, TestValue};
    use std::sync::atomic::AtomicU32;

    fn counter_prefix() -> Arc<PrefixData> {
        Arc::new(PrefixData {
            code: "let n = 0\nexport function bump() { return ++n }".to_string(),
            filename: None,
            globals: Vec::new(),
            declared_globals: Vec::new(),
            declared_imports: Vec::new(),
        })
    }

    fn test_brand_key() -> String {
        crate::webcodec::brand_key_for_token(&[0xab; crate::webcodec::DESCRIPTOR_TOKEN_LEN])
    }

    fn bump_job() -> CallJob {
        CallJob {
            token: 0,
            code: None,
            filename: None,
            limits: sandbox::Limits::default(),
            globals: Vec::new(),
            io: sandbox::RunIo::None,
            call_id_counter: Arc::new(AtomicU32::new(0)),
            call: Some(crate::ipc::CallSpec {
                export_path: "bump".to_string(),
                args_blob: testval::to_blob(&TestValue::Array(Vec::new())),
            }),
            epilogue: None,
            complete: None,
            ctl_slot: None,
        }
    }

    fn bump_value(outcome: &sandbox::CallOutcome) -> f64 {
        let out = outcome.result.as_ref().expect("bump succeeds");
        match testval::from_blob(&out.exports) {
            TestValue::Number(n) => n,
            other => panic!("expected a number, got {other:?}"),
        }
    }

    #[test]
    fn instance_thread_serves_calls_and_keeps_state() {
        sandbox::init_platform();
        let handle = spawn_instance(counter_prefix(), 0, test_brand_key()).expect("spawn instance");
        for expected in 1..=3 {
            let outcome = handle.call(bump_job());
            assert!(!outcome.tainted);
            assert_eq!(bump_value(&outcome), f64::from(expected));
        }
    }

    const TEST_MB: u64 = 1024 * 1024;
    /// A plausible per-run heap cap for tests exercising the latch, not the
    /// admission line (registries here use hard line 0 unless stated).
    const TEST_CAP: u64 = 128 * TEST_MB;

    fn acquire(reg: &WarmRegistry, prefix: &str, cap: u64) -> Acquired {
        reg.acquire(prefix, cap, &|| {
            spawn_instance(counter_prefix(), 0, test_brand_key())
        })
    }

    fn att(acquired: Acquired) -> AttachedInstance {
        match acquired {
            Acquired::Attached(a) => a,
            Acquired::Cold(_) => panic!("expected Attached, got Cold"),
            Acquired::Refused(msg) => panic!("expected Attached, got Refused: {msg}"),
            Acquired::SpawnFailed(e) => panic!("expected Attached, got SpawnFailed: {e}"),
        }
    }

    fn call_via(a: &AttachedInstance, job: CallJob) -> sandbox::CallOutcome {
        let (tx, rx) = crossbeam_channel::bounded(1);
        a.sender()
            .send((Box::new(job), Some(tx)))
            .expect("instance accepts the job");
        rx.recv().expect("instance answers")
    }

    /// Release with the outcome's real telemetry (a clean prefix run).
    fn release_outcome(
        reg: &WarmRegistry,
        prefix: &str,
        a: &AttachedInstance,
        out: &sandbox::CallOutcome,
    ) {
        let cpu = match &out.result {
            Ok(o) => o.cpu_time_ms,
            Err(f) => f.cpu_time_ms,
        };
        reg.release(prefix, a.id, out.tainted, out.heap_used_bytes, cpu, true);
    }

    #[test]
    fn registry_reuses_a_released_instance() {
        sandbox::init_platform();
        let registry = WarmRegistry::new(0, 0);
        let a1 = att(acquire(&registry, "p0", TEST_CAP));
        let outcome = call_via(&a1, bump_job());
        assert_eq!(bump_value(&outcome), 1.0);
        release_outcome(&registry, "p0", &a1, &outcome);

        // Same prefix again: the idle instance comes back, state intact.
        let a2 = att(acquire(&registry, "p0", TEST_CAP));
        assert_eq!(a2.id, a1.id, "the idle instance must be reused");
        let outcome = call_via(&a2, bump_job());
        assert_eq!(bump_value(&outcome), 2.0);
        release_outcome(&registry, "p0", &a2, &outcome);
    }

    #[test]
    fn tainted_release_discards_the_instance() {
        sandbox::init_platform();
        let registry = WarmRegistry::new(0, 0);
        let a1 = att(acquire(&registry, "p0", TEST_CAP));
        let outcome = call_via(&a1, bump_job());
        registry.release("p0", a1.id, true, outcome.heap_used_bytes, 0.0, true);

        // Nothing idle — the next acquire spawns fresh, state reset.
        let a2 = att(acquire(&registry, "p0", TEST_CAP));
        assert_ne!(a2.id, a1.id, "a tainted instance must not be reused");
        let outcome = call_via(&a2, bump_job());
        assert_eq!(bump_value(&outcome), 1.0, "fresh instance, fresh state");
        release_outcome(&registry, "p0", &a2, &outcome);
    }

    // ── Join vs spawn (#77) ────────────────────────────────────────────────
    //
    // Accounting-only: instances are spawned but never called (the owner
    // thread builds its isolate lazily), and the demand EWMAs are fed with
    // hand-picked telemetry through `release`.

    #[test]
    fn a_waiting_heavy_prefix_joins_its_busy_instance() {
        let registry = WarmRegistry::new(0, 0);
        // Seed: one completed run with ~zero CPU — demand rate × 0 = 0.
        let a1 = att(acquire(&registry, "p", TEST_CAP));
        registry.release("p", a1.id, false, TEST_MB, 0.0, true);

        // The instance goes busy again, then a third run arrives while it
        // is held: no idle, demand says waiting → JOIN the same instance.
        let a2 = att(acquire(&registry, "p", TEST_CAP));
        assert_eq!(a2.id, a1.id);
        let a3 = att(acquire(&registry, "p", TEST_CAP));
        assert_eq!(a3.id, a2.id, "waiting-heavy runs join, never spawn");

        let stats = registry.stats();
        assert_eq!(stats.warm_busy, 1, "one instance carries both runs");
        assert_eq!(stats.warm_runs, 2);
        registry.release("p", a2.id, false, TEST_MB, 0.0, true);
        registry.release("p", a3.id, false, TEST_MB, 0.0, true);
        assert_eq!(registry.stats().warm_idle, 1);
    }

    #[test]
    fn a_compute_heavy_prefix_opens_another_instance() {
        let registry = WarmRegistry::new(0, 0);
        // Seed: a completed run that was pure CPU — any real arrival rate
        // makes the demand exceed one instance.
        let a1 = att(acquire(&registry, "p", TEST_CAP));
        registry.release("p", a1.id, false, TEST_MB, 1_000.0, true);

        let a2 = att(acquire(&registry, "p", TEST_CAP));
        assert_eq!(a2.id, a1.id, "idle reuse still wins");
        let a3 = att(acquire(&registry, "p", TEST_CAP));
        assert_ne!(a3.id, a2.id, "CPU demand must open a second isolate");
        assert_eq!(registry.stats().warm_busy, 2);
        registry.release("p", a2.id, false, TEST_MB, 1_000.0, true);
        registry.release("p", a3.id, false, TEST_MB, 1_000.0, true);
    }

    #[test]
    fn an_unmeasured_prefix_spawns_like_today() {
        // No completed run yet: unknown CPU must not serialize a possibly
        // compute-heavy prefix — every concurrent run gets its own isolate,
        // exactly the pre-#77 behavior.
        let registry = WarmRegistry::new(0, 0);
        let a1 = att(acquire(&registry, "p", TEST_CAP));
        let a2 = att(acquire(&registry, "p", TEST_CAP));
        assert_ne!(a1.id, a2.id);
        registry.release("p", a1.id, false, 0, 0.0, true);
        registry.release("p", a2.id, false, 0, 0.0, true);
    }

    #[test]
    fn the_memory_wall_joins_instead_of_refusing() {
        // Spawn demanded but the admission line refuses the isolate: the
        // run joins the busy instance — joining is never memory-gated.
        let registry = WarmRegistry::new(0, 900 * TEST_MB);
        registry.set_usage_for_test(700 * TEST_MB);
        let cap = 64 * TEST_MB;
        let a1 = att(acquire(&registry, "p", cap));
        registry.release("p", a1.id, false, cap, 1_000.0, true);

        let a2 = att(acquire(&registry, "p", cap));
        assert_eq!(a2.id, a1.id);
        // Now a further isolate would cross the line (850 + 64 > 900).
        registry.set_usage_for_test(850 * TEST_MB);
        let a3 = att(acquire(&registry, "p", cap));
        assert_eq!(a3.id, a2.id, "over the line the run joins, never fails");
        registry.release("p", a2.id, false, cap, 1_000.0, true);
        registry.release("p", a3.id, false, cap, 1_000.0, true);
    }

    #[test]
    fn a_cap_mismatch_never_joins() {
        // #81 r6: attach refuses a memoryMb mismatch — the mismatched run
        // gets its own isolate instead (unreachable today with the uniform
        // sandbox cap; the invariant is pinned for the per-prefix caps).
        let registry = WarmRegistry::new(0, 0);
        let a1 = att(acquire(&registry, "p", 64 * TEST_MB));
        let a2 = att(acquire(&registry, "p", 128 * TEST_MB));
        assert_ne!(a1.id, a2.id);
        registry.release("p", a1.id, false, 0, 0.0, true);
        registry.release("p", a2.id, false, 0, 0.0, true);
    }

    // ── Admission line (#77 commit 2) ──────────────────────────────────────

    #[test]
    fn the_admission_line_refuses_new_isolates_but_never_reuse() {
        // Hard line 900 MB, usage 800: a 128 MB isolate no longer fits
        // (928 > 900) while a 64 MB one does.
        let registry = WarmRegistry::new(0, 900 * TEST_MB);
        registry.set_usage_for_test(800 * TEST_MB);
        let Acquired::Refused(msg) = acquire(&registry, "p", TEST_CAP) else {
            panic!("128 MB run at 800/900 MB must be refused");
        };
        assert!(msg.contains("admission line"), "message names the line: {msg}");
        let small = att(acquire(&registry, "p", 64 * TEST_MB));
        registry.release("p", small.id, false, 0, 0.0, true);

        // Reuse is never admission-gated: the idle 64 MB instance serves
        // its next run even though a new isolate would be refused.
        registry.set_usage_for_test(890 * TEST_MB);
        let again = att(acquire(&registry, "p", 64 * TEST_MB));
        assert_eq!(again.id, small.id);

        let stats = registry.stats();
        assert_eq!(stats.oneoff_running, 0);
        assert_eq!(stats.warm_busy, 1);
        assert_eq!(stats.hard_line_bytes, 900 * TEST_MB);
        registry.release("p", again.id, false, 0, 0.0, true);
    }

    #[test]
    fn one_off_runs_hit_the_same_admission_line() {
        let registry = WarmRegistry::new(0, 900 * TEST_MB);
        registry.set_usage_for_test(800 * TEST_MB);
        assert!(registry.reserve_oneoff(TEST_CAP).is_err());
        assert_eq!(registry.stats().oneoff_running, 0, "refusal takes no counters");
        registry.reserve_oneoff(64 * TEST_MB).expect("a smaller run fits");
        assert_eq!(registry.stats().oneoff_running, 1);
        registry.release_oneoff();
    }

    #[test]
    fn an_uncapped_run_is_refused_from_the_budget_mark() {
        // memoryMb: 0 has no arithmetic against the line — the budget is
        // its refusal mark (ruled 2026-09-03). Below it: admitted.
        let registry = WarmRegistry::new(100 * TEST_MB, 900 * TEST_MB);
        registry.set_usage_for_test(50 * TEST_MB);
        let a = att(acquire(&registry, "p", 0));
        registry.release("p", a.id, false, 0, 0.0, true);

        registry.set_usage_for_test(100 * TEST_MB);
        // "p"'s idle instance was shed by the pass above the mark; a fresh
        // uncapped isolate for "q" must be refused.
        let Acquired::Refused(msg) = acquire(&registry, "q", 0) else {
            panic!("uncapped run at the budget must be refused");
        };
        assert!(msg.contains("uncapped"), "message names the cause: {msg}");
        assert!(registry.reserve_oneoff(0).is_err(), "one-offs identically");
    }

    // ── Watermarks and eviction ────────────────────────────────────────────

    #[test]
    fn shedding_degenerates_to_lru_with_equal_heaps() {
        // Zero heaps → every score is 0 → the pick falls back to the
        // longest-idle, i.e. plain LRU, exactly the celld order.
        let registry = WarmRegistry::new(100 * TEST_MB, 0);
        registry.set_usage_for_test(10 * TEST_MB);
        let a = att(acquire(&registry, "a", TEST_CAP));
        registry.release("a", a.id, false, 0, 0.0, true);
        let b = att(acquire(&registry, "b", TEST_CAP));
        registry.release("b", b.id, false, 0, 0.0, true);

        // Usage reaches the mark: the pass sheds one (2 idle / 10 → min 1),
        // and it must be "a" — released first, idle longest.
        registry.set_usage_for_test(100 * TEST_MB);
        registry.reserve_oneoff(TEST_CAP).unwrap();
        registry.release_oneoff();
        let stats = registry.stats();
        assert_eq!(stats.warm_idle, 1);
        assert_eq!(stats.per_prefix, vec![("b".to_string(), 1, 0)]);
    }

    #[test]
    fn without_a_budget_nothing_is_ever_evicted() {
        // Watermarks disabled (budget 0) and no count cap exists: any
        // number of prefixes stays resident, one-off traffic evicts nothing.
        let registry = WarmRegistry::new(0, 0);
        let mut ids = Vec::new();
        for prefix in ["a", "b", "c", "d"] {
            let a = att(acquire(&registry, prefix, TEST_CAP));
            ids.push((prefix, a.id));
            registry.release(prefix, a.id, false, 0, 0.0, true);
        }
        registry.reserve_oneoff(TEST_CAP).unwrap();
        registry.release_oneoff();
        for (prefix, id) in ids {
            let again = att(acquire(&registry, prefix, TEST_CAP));
            assert_eq!(again.id, id, "warmth survives untouched");
            registry.release(prefix, id, false, 0, 0.0, true);
        }
    }

    #[test]
    fn shedding_evicts_the_highest_scored_victim() {
        // Budget 100 MB = the mark; release line 80 MB. Two idle instances:
        // "big" released later with a 1 GB heap claim, "small" earlier with
        // 1 KB — the score (heap × idle) must pick "big" despite its youth.
        let registry = WarmRegistry::new(100 * TEST_MB, 0);
        registry.set_usage_for_test(10 * TEST_MB);
        let small = att(acquire(&registry, "small", TEST_CAP));
        registry.release("small", small.id, false, 1_000, 0.0, true);
        let big = att(acquire(&registry, "big", TEST_CAP));
        registry.release("big", big.id, false, 1024 * TEST_MB, 0.0, true);

        registry.set_usage_for_test(100 * TEST_MB);
        // Any registry event runs the pressure pass: idle 2 → shed 1.
        registry.reserve_oneoff(TEST_CAP).unwrap();
        let stats = registry.stats();
        assert!(stats.under_pressure);
        assert_eq!(stats.warm_idle, 1);
        assert_eq!(stats.per_prefix, vec![("small".to_string(), 1, 0)]);

        // Same usage on the next event: flat sample → futility stops the
        // walk, "small" survives.
        registry.release_oneoff();
        assert_eq!(registry.stats().warm_idle, 1);

        // The latch holds between the release line (80) and the mark, and
        // a moved sample re-arms the walk — it takes the last idle one.
        registry.set_usage_for_test(90 * TEST_MB);
        registry.reserve_oneoff(TEST_CAP).unwrap();
        registry.release_oneoff();
        let stats = registry.stats();
        assert!(stats.under_pressure);
        assert_eq!(stats.warm_idle, 0);

        registry.set_usage_for_test(80 * TEST_MB);
        registry.reserve_oneoff(TEST_CAP).unwrap();
        registry.release_oneoff();
        assert!(!registry.stats().under_pressure);
    }

    #[test]
    fn shedding_degrades_new_warmth_to_cold_but_reuses_existing() {
        let registry = WarmRegistry::new(100 * TEST_MB, 0);
        registry.set_usage_for_test(10 * TEST_MB);
        let p = att(acquire(&registry, "p", TEST_CAP));
        registry.release("p", p.id, false, 1_000, 0.0, true);
        let q = att(acquire(&registry, "q", TEST_CAP));
        registry.release("q", q.id, false, 1024 * TEST_MB, 0.0, true);

        // At the mark: the pass takes "q" (highest score); reuse of "p"'s
        // existing warmth stays allowed — it adds no memory…
        registry.set_usage_for_test(100 * TEST_MB);
        let p_again = att(acquire(&registry, "p", TEST_CAP));
        assert_eq!(p_again.id, p.id);
        // …but nothing NEW may be pooled while the latch holds: "q" lost
        // its instance and degrades to a cold one-off (celld: a pressured
        // node serves on what it has, but must not build another heap).
        let Acquired::Cold(handle) = acquire(&registry, "q", TEST_CAP) else {
            panic!("under the latch a prefix without warmth goes cold");
        };
        let stats = registry.stats();
        assert_eq!(stats.oneoff_running, 1);
        assert_eq!(stats.warm_busy, 1);
        drop(handle);
        registry.release_oneoff();
        registry.release("p", p.id, false, TEST_MB, 0.0, true);

        // Latch released (at/below 80% of the mark): normal pooling.
        registry.set_usage_for_test(50 * TEST_MB);
        assert!(matches!(acquire(&registry, "r", TEST_CAP), Acquired::Attached(_)));
    }

    #[test]
    fn cold_accounting_balances_the_ledger() {
        let registry = WarmRegistry::new(100 * TEST_MB, 0);
        registry.set_usage_for_test(100 * TEST_MB);
        let Acquired::Cold(handle) = acquire(&registry, "p", TEST_CAP) else {
            panic!("under the latch the first isolate must be cold");
        };
        assert_eq!(registry.stats().oneoff_running, 1);
        drop(handle);
        registry.release_oneoff();
        let stats = registry.stats();
        assert_eq!(stats.oneoff_running, 0);
        assert_eq!(stats.warm_busy, 0);
        assert_eq!(stats.warm_idle, 0);
        assert!(stats.per_prefix.is_empty());
    }

    #[test]
    fn stats_snapshot_counts_idle_busy_and_oneoff() {
        let registry = WarmRegistry::new(0, 0);
        // Two instances of "a" (unmeasured prefix → concurrent runs spawn):
        // one stays busy, one is released idle with a known heap. Plus one
        // running one-off.
        let a1 = att(acquire(&registry, "a", TEST_CAP));
        let a2 = att(acquire(&registry, "a", TEST_CAP));
        assert_ne!(a1.id, a2.id);
        registry.release("a", a2.id, false, 1_000, 0.0, true);
        registry.reserve_oneoff(TEST_CAP).unwrap();

        let stats = registry.stats();
        assert_eq!(stats.oneoff_running, 1);
        assert_eq!(stats.warm_busy, 1);
        assert_eq!(stats.warm_runs, 1);
        assert_eq!(stats.warm_idle, 1);
        assert_eq!(stats.idle_heap_bytes, 1_000);
        assert_eq!(stats.warm_budget_bytes, 0);
        assert_eq!(stats.per_prefix, vec![("a".to_string(), 1, 1)]);

        // Drain everything (tainted, so nothing is pooled): counts return
        // to zero and the per-prefix row disappears.
        registry.release_oneoff();
        registry.release("a", a1.id, true, 0, 0.0, true);
        let a3 = att(acquire(&registry, "a", TEST_CAP));
        registry.release("a", a3.id, true, 0, 0.0, true);
        let stats = registry.stats();
        assert_eq!(stats.oneoff_running, 0);
        assert_eq!(stats.warm_busy, 0);
        assert_eq!(stats.warm_runs, 0);
        assert_eq!(stats.warm_idle, 0);
        assert_eq!(stats.idle_heap_bytes, 0);
        assert!(stats.per_prefix.is_empty());
    }

    #[test]
    fn stats_stay_consistent_after_an_eviction() {
        let registry = WarmRegistry::new(100 * TEST_MB, 0);
        registry.set_usage_for_test(10 * TEST_MB);
        let a = att(acquire(&registry, "a", TEST_CAP));
        registry.release("a", a.id, false, 500, 0.0, true);

        // Usage reaches the mark: the one-off's pressure pass evicts a's
        // idle instance — every count reflects that immediately.
        registry.set_usage_for_test(100 * TEST_MB);
        registry.reserve_oneoff(TEST_CAP).unwrap();
        let stats = registry.stats();
        assert_eq!(stats.warm_idle, 0);
        assert_eq!(stats.idle_heap_bytes, 0);
        assert_eq!(stats.oneoff_running, 1);
        assert!(stats.per_prefix.is_empty());
        registry.release_oneoff();

        // Pressure gone: pooling resumes and the counts follow.
        registry.set_usage_for_test(10 * TEST_MB);
        let b = att(acquire(&registry, "b", TEST_CAP));
        registry.release("b", b.id, false, 300, 0.0, true);
        let stats = registry.stats();
        assert_eq!(stats.warm_idle, 1);
        assert_eq!(stats.warm_busy, 0);
        assert_eq!(stats.idle_heap_bytes, 300);
    }

    #[test]
    fn dispose_prefix_drops_idle_instances() {
        let registry = WarmRegistry::new(0, 0);
        let a = att(acquire(&registry, "p0", TEST_CAP));
        registry.release("p0", a.id, false, 0, 0.0, true);
        registry.dispose_prefix("p0");
        let fresh = att(acquire(&registry, "p0", TEST_CAP));
        assert_ne!(fresh.id, a.id, "disposed warmth must not serve");
        registry.release("p0", fresh.id, false, 0, 0.0, true);
    }

    #[test]
    fn dispose_prefix_with_a_busy_instance_keeps_its_accounting() {
        let registry = WarmRegistry::new(0, 0);
        let a = att(acquire(&registry, "p0", TEST_CAP));
        // Dispose lands while the instance is busy: nothing idle to drop
        // yet, no new joins (dead), the release drains it.
        registry.dispose_prefix("p0");
        let stats = registry.stats();
        assert_eq!(stats.warm_busy, 1);
        registry.release("p0", a.id, false, 0, 0.0, false);
        let stats = registry.stats();
        assert_eq!(stats.warm_busy, 0);
        assert!(stats.per_prefix.is_empty());
    }

    // ── The per-instance turn loop: interleaved session runs ──────────────
    //
    // These drive `serve_instance` the way the session demux does: jobs with
    // channel I/O, bridge frames answered by the test, outcomes read off the
    // respond channels. One instance, several runs in flight.

    use std::os::unix::net::UnixStream;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    fn interleave_prefix() -> Arc<PrefixData> {
        Arc::new(PrefixData {
            code: "let n = 0\n\
                   export function bump() { return ++n }\n\
                   export async function viaTool(x) { console.log('run-' + x); const v = await tool(); return [x, v] }\n\
                   export async function hangBump() { n++; await tool(); return n }\n\
                   export function spin() { for (;;) {} }\n\
                   export function busy(c) { let x = 0; for (let i = 0; i < c; i++) x = (x + i) & 1048575; return x }"
                .to_string(),
            filename: None,
            globals: Vec::new(),
            declared_globals: vec!["tool".to_string()],
            declared_imports: Vec::new(),
        })
    }

    struct SessionRun {
        /// The run's table token — events into the instance's shared channel
        /// are tagged with it, exactly as the session demux tags them.
        token: u64,
        events: sandbox::RunEventSender,
        outcome: crossbeam_channel::Receiver<sandbox::CallOutcome>,
    }

    impl SessionRun {
        fn send(&self, event: sandbox::RunEvent) {
            self.events
                .send(sandbox::RoutedEvent::new(self.token, event))
                .unwrap();
        }
    }

    /// Submit one channel-mode job calling `export_path(args)` with a live
    /// `tool` bridge stub writing to `sink_fd`'s socket.
    #[allow(clippy::too_many_arguments)]
    fn submit_session_job(
        handle: &InstanceHandle,
        run_id: u32,
        export_path: &str,
        args: Vec<TestValue>,
        sink: crate::ipc::FrameSink,
        counter: &Arc<AtomicU32>,
        limits: sandbox::Limits,
    ) -> SessionRun {
        let token = sandbox::alloc_run_token();
        let (otx, orx) = crossbeam_channel::bounded(1);
        let job = Box::new(CallJob {
            token,
            code: None,
            filename: None,
            limits,
            globals: vec![crate::ipc::HostGlobalDef::bridge("tool")],
            io: sandbox::RunIo::Instance { sink },
            call_id_counter: Arc::clone(counter),
            call: Some(crate::ipc::CallSpec {
                export_path: export_path.to_string(),
                args_blob: testval::to_blob(&TestValue::Array(args)),
            }),
            epilogue: Some(sandbox::EpilogueSpec {
                run_id,
                report_heap: false,
            }),
            complete: None,
            ctl_slot: None,
        });
        handle
            .sender()
            .send((job, Some(otx)))
            .expect("instance accepts the job");
        SessionRun {
            token,
            events: handle.event_sender(),
            outcome: orx,
        }
    }

    /// Read one BridgeCall frame off the writer socket: (runId, callId).
    fn read_bridge_call(server: &mut UnixStream) -> (u32, u32) {
        let frame = crate::ipc::read_rust_to_ts_frame(server).expect("a BridgeCall frame");
        assert_eq!(frame.message_type, crate::ipc::RustToTsMessageType::BridgeCall);
        let run_id = u32::from_be_bytes(frame.payload[0..4].try_into().unwrap());
        let call_id = u32::from_be_bytes(frame.payload[4..8].try_into().unwrap());
        (run_id, call_id)
    }

    /// A successful BridgeResponse frame event (runId, callId, number value).
    fn bridge_response_event(run_id: u32, call_id: u32, value: f64) -> sandbox::RunEvent {
        let mut p = Vec::new();
        p.extend_from_slice(&run_id.to_be_bytes());
        p.extend_from_slice(&call_id.to_be_bytes());
        p.push(1); // ok
        p.push(1); // value present
        let blob = testval::to_blob(&TestValue::Number(value));
        p.extend_from_slice(&(blob.len() as u32).to_be_bytes());
        p.extend_from_slice(&blob);
        sandbox::RunEvent::Frame(crate::ipc::TypedFrame {
            message_type: crate::ipc::TsToRustMessageType::BridgeResponse,
            payload: p,
        })
    }

    #[test]
    fn one_instance_interleaves_two_runs_with_correct_attribution() {
        sandbox::init_platform();
        let (mut server, client) = UnixStream::pair().unwrap();
        let sink = crate::ipc::FrameSink::Shared(crate::ipc::Outbox::spawn(&client).unwrap());
        let handle = spawn_instance(interleave_prefix(), 0, test_brand_key()).expect("spawn instance");
        let counter = Arc::new(AtomicU32::new(0));

        // Two runs, both suspended awaiting their bridge response.
        let run1 = submit_session_job(
            &handle, 101, "viaTool", vec![TestValue::Number(1.0)],
            sink.clone(), &counter, sandbox::Limits::default(),
        );
        let run2 = submit_session_job(
            &handle, 102, "viaTool", vec![TestValue::Number(2.0)],
            sink.clone(), &counter, sandbox::Limits::default(),
        );

        // Both BridgeCall frames arrive on the one serialized writer, each
        // leading with its run id.
        let first = read_bridge_call(&mut server);
        let second = read_bridge_call(&mut server);
        let mut calls = std::collections::HashMap::from([first, second]);
        assert_eq!(calls.len(), 2, "two distinct runs sent bridge calls");
        let c1 = calls.remove(&101).expect("run 101 sent a call");
        let c2 = calls.remove(&102).expect("run 102 sent a call");

        // Answer run 2 FIRST: routing must deliver each response to its own
        // run even though run 1 suspended earlier.
        run2.send(bridge_response_event(102, c2, 20.0));
        let out2 = run2.outcome.recv().expect("run 2 completes");
        run1.send(bridge_response_event(101, c1, 10.0));
        let out1 = run1.outcome.recv().expect("run 1 completes");

        let v1 = out1.result.expect("run 1 ok");
        let v2 = out2.result.expect("run 2 ok");
        assert!(!out1.tainted && !out2.tainted);
        assert_eq!(
            testval::from_blob(&v1.exports),
            TestValue::Array(vec![TestValue::Number(1.0), TestValue::Number(10.0)])
        );
        assert_eq!(
            testval::from_blob(&v2.exports),
            TestValue::Array(vec![TestValue::Number(2.0), TestValue::Number(20.0)])
        );
        // Console attribution: each run's lines land in ITS result, even
        // though both ran turns on one shared instance.
        assert_eq!(v1.stdout, vec!["run-1".to_string()]);
        assert_eq!(v2.stdout, vec!["run-2".to_string()]);
    }

    #[test]
    fn a_mid_turn_kill_resets_innocent_co_resident_runs() {
        sandbox::init_platform();
        let (mut server, client) = UnixStream::pair().unwrap();
        let sink = crate::ipc::FrameSink::Shared(crate::ipc::Outbox::spawn(&client).unwrap());
        let handle = spawn_instance(interleave_prefix(), 0, test_brand_key()).expect("spawn instance");
        let counter = Arc::new(AtomicU32::new(0));

        // The innocent victim: suspended awaiting a bridge response.
        let victim = submit_session_job(
            &handle, 8, "viaTool", vec![TestValue::Number(1.0)],
            sink.clone(), &counter, sandbox::Limits::default(),
        );
        let _ = read_bridge_call(&mut server); // proof it is suspended

        // The culprit: a synchronous spin killed mid-turn by the CPU guard.
        let culprit = submit_session_job(
            &handle, 7, "spin", vec![],
            sink.clone(), &counter,
            sandbox::Limits {
                cpu_time_ms: 100,
                wall_time_ms: 10_000,
                ..Default::default()
            },
        );

        let culprit_out = culprit.outcome.recv().expect("culprit concludes");
        assert!(culprit_out.tainted);
        assert!(matches!(
            culprit_out.result.unwrap_err().error,
            sandbox::RunError::CpuTimeout
        ));

        let victim_out = victim.outcome.recv().expect("victim concludes");
        assert!(victim_out.tainted, "the whole instance is untrustworthy");
        match victim_out.result.unwrap_err().error {
            sandbox::RunError::InstanceReset {
                cause,
                culprit_run_id,
            } => {
                assert_eq!(cause, sandbox::ResetCause::Cpu);
                assert_eq!(culprit_run_id, 7, "the culprit's wire run id is named");
            }
            other => panic!("expected InstanceReset, got {other:?}"),
        }
    }

    #[test]
    fn an_abort_abandons_the_run_and_the_instance_survives() {
        sandbox::init_platform();
        let (mut server, client) = UnixStream::pair().unwrap();
        let sink = crate::ipc::FrameSink::Shared(crate::ipc::Outbox::spawn(&client).unwrap());
        let handle = spawn_instance(interleave_prefix(), 0, test_brand_key()).expect("spawn instance");
        let counter = Arc::new(AtomicU32::new(0));

        // Suspend a run mid-bridge after it bumped the counter, then abort.
        let run = submit_session_job(
            &handle, 5, "hangBump", vec![],
            sink.clone(), &counter, sandbox::Limits::default(),
        );
        let _ = read_bridge_call(&mut server);
        run.send(sandbox::RunEvent::Frame(crate::ipc::TypedFrame {
            message_type: crate::ipc::TsToRustMessageType::Terminate,
            payload: 5u32.to_be_bytes().to_vec(),
        }));
        let aborted = run.outcome.recv().expect("aborted run concludes");
        assert!(
            !aborted.tainted,
            "a boundary abort abandons the run without tainting the instance"
        );
        assert!(matches!(
            aborted.result.unwrap_err().error,
            sandbox::RunError::Aborted
        ));

        // The instance keeps serving, abandoned side effects intact: the
        // n++ before the hang is visible, and its continuation never ran.
        let outcome = handle.call(bump_job());
        assert!(!outcome.tainted);
        assert_eq!(bump_value(&outcome), 2.0, "n was 1 from hangBump, bump → 2");
    }

    #[test]
    fn a_boundary_wall_expiry_does_not_taint_the_instance() {
        // The run is suspended awaiting a bridge response when its wall
        // budget runs out: a loop-deadline failure, not a mid-JS kill — the
        // instance stays trustworthy and keeps its state (E1 ruling 3
        // deliberately narrows the old taint-on-WallTimeout).
        sandbox::init_platform();
        let (mut server, client) = UnixStream::pair().unwrap();
        let sink = crate::ipc::FrameSink::Shared(crate::ipc::Outbox::spawn(&client).unwrap());
        let handle = spawn_instance(interleave_prefix(), 0, test_brand_key()).expect("spawn instance");
        let counter = Arc::new(AtomicU32::new(0));

        let run = submit_session_job(
            &handle, 9, "hangBump", vec![],
            sink.clone(), &counter,
            sandbox::Limits {
                wall_time_ms: 200,
                ..Default::default()
            },
        );
        let _ = read_bridge_call(&mut server);
        let out = run.outcome.recv().expect("run concludes at its wall");
        assert!(!out.tainted, "boundary wall expiry must not taint");
        assert!(matches!(
            out.result.unwrap_err().error,
            sandbox::RunError::WallTimeout
        ));

        // Reuse: same instance, state intact.
        let outcome = handle.call(bump_job());
        assert!(!outcome.tainted);
        assert_eq!(bump_value(&outcome), 2.0);
    }

    /// A benign frame for run 22: a StreamChunk for a stream it never opened
    /// is ignored, but delivering it still costs a turn.
    fn ignored_chunk_for_22() -> sandbox::RunEvent {
        let mut chunk = Vec::new();
        chunk.extend_from_slice(&22u32.to_be_bytes()); // run id
        chunk.extend_from_slice(&999u32.to_be_bytes()); // unknown stream id
        chunk.extend_from_slice(&1u32.to_be_bytes()); // 1 data byte
        chunk.push(0);
        sandbox::RunEvent::Frame(crate::ipc::TypedFrame {
            message_type: crate::ipc::TsToRustMessageType::StreamChunk,
            payload: chunk,
        })
    }

    #[test]
    fn continuous_frame_traffic_does_not_starve_a_co_resident_deadline() {
        // The E2 review's starvation hazard (carried item 8): the loop used
        // to observe deadlines only when its select TIMED OUT, and a select
        // with a non-empty event queue never times out — so traffic to one
        // run could carry an expired co-resident arbitrarily far past its
        // wall. Under the arrival-order rule the wall must fire as soon as
        // the loop reaches the first event that arrived after it.
        //
        // Shape: suspend A (short wall) and B (no wall), then pump benign
        // frames at B from a producer thread that far outruns the loop's
        // drain rate. A must conclude WallTimeout while the queue is still
        // non-empty — the old code could only conclude it on a drained-empty
        // queue.
        sandbox::init_platform();
        let (mut server, client) = UnixStream::pair().unwrap();
        let sink = crate::ipc::FrameSink::Shared(crate::ipc::Outbox::spawn(&client).unwrap());
        let handle = spawn_instance(interleave_prefix(), 0, test_brand_key()).expect("spawn instance");
        let counter = Arc::new(AtomicU32::new(0));

        let b = submit_session_job(
            &handle, 22, "viaTool", vec![TestValue::Number(2.0)],
            sink.clone(), &counter, sandbox::Limits::default(),
        );
        let (b_run, b_call) = read_bridge_call(&mut server);
        assert_eq!(b_run, 22);

        let a = submit_session_job(
            &handle, 21, "hangBump", vec![],
            sink.clone(), &counter,
            sandbox::Limits {
                wall_time_ms: 50,
                ..Default::default()
            },
        );
        let (a_run, _a_call) = read_bridge_call(&mut server);
        assert_eq!(a_run, 21);

        // Pump until A concluded (or a cap: enqueueing is ~10× faster than a
        // turn, so the cap lasts far past the wall at any plausible ratio).
        // Assumes the feeder thread gets scheduled within A's 50 ms wall —
        // it is runnable the whole time, so only a fully starved scheduler
        // breaks that; revisit if this ever flakes on loaded CI.
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let feeder = {
            let stop = Arc::clone(&stop);
            let events = handle.event_sender();
            let token = b.token;
            std::thread::spawn(move || {
                for _ in 0..600_000 {
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    events
                        .send(sandbox::RoutedEvent::new(token, ignored_chunk_for_22()))
                        .unwrap();
                }
            })
        };

        let out = a
            .outcome
            .recv_timeout(Duration::from_secs(10))
            .expect("run A concludes at its wall despite B's traffic");
        let backlog_at_conclusion = !b.events.is_empty();
        stop.store(true, Ordering::Relaxed);
        feeder.join().unwrap();

        assert!(matches!(
            out.result.unwrap_err().error,
            sandbox::RunError::WallTimeout
        ));
        assert!(!out.tainted, "boundary wall expiry must not taint");
        assert!(
            backlog_at_conclusion,
            "A's wall fired only once B's traffic fully drained — deadline starvation"
        );

        // The instance stayed healthy throughout: settle B normally (its
        // response queues behind the leftover traffic and drains through).
        b.send(bridge_response_event(22, b_call, 20.0));
        let out_b = b.outcome.recv().expect("run B completes");
        assert!(!out_b.tainted);
    }

    #[test]
    fn a_response_that_arrived_in_time_beats_the_wall_deadline() {
        // The inverse guarantee of the arrival-order rule: a settling frame
        // that reached the demux BEFORE the run's wall deadline must be
        // delivered even when the loop only gets to it after the deadline
        // has passed — a busy engine must not turn an answered run into a
        // WallTimeout.
        //
        // Shape: suspend A on a bridge call (wall 150 ms), then occupy the
        // loop with one long CPU turn (run C) and enqueue A's response
        // while C spins — stamped well before A's wall. The loop resumes
        // after A's wall has passed; A must still settle with its value.
        sandbox::init_platform();
        let (mut server, client) = UnixStream::pair().unwrap();
        let sink = crate::ipc::FrameSink::Shared(crate::ipc::Outbox::spawn(&client).unwrap());
        let handle = spawn_instance(interleave_prefix(), 0, test_brand_key()).expect("spawn instance");
        let counter = Arc::new(AtomicU32::new(0));

        let started = Instant::now();
        let a = submit_session_job(
            &handle, 21, "hangBump", vec![],
            sink.clone(), &counter,
            sandbox::Limits {
                wall_time_ms: 150,
                ..Default::default()
            },
        );
        let (a_run, a_call) = read_bridge_call(&mut server);
        assert_eq!(a_run, 21);

        // One long-running start turn. The busy count is calibrated far
        // above the wall on any machine; the elapsed assert below fails
        // loudly (rather than passing vacuously) if it ever gets too fast.
        let c = submit_session_job(
            &handle, 23, "busy", vec![TestValue::Number(1_500_000_000.0)],
            sink.clone(), &counter, sandbox::Limits::default(),
        );
        // Give the loop a moment to dispatch C — its job is the only ready
        // operation until the response below is enqueued.
        std::thread::sleep(Duration::from_millis(10));
        a.send(bridge_response_event(21, a_call, 10.0));

        let out_c = c.outcome.recv_timeout(Duration::from_secs(30)).expect("C completes");
        assert!(!out_c.tainted);
        out_c.result.expect("busy() succeeds");
        assert!(
            started.elapsed() > Duration::from_millis(160),
            "busy() finished before A's wall — raise its iteration count, \
             the scenario no longer exercises the deadline boundary"
        );

        let out_a = a
            .outcome
            .recv_timeout(Duration::from_secs(5))
            .expect("run A concludes");
        let v = out_a.result.expect(
            "A's response arrived before its wall deadline and must settle it — \
             a busy loop must not turn an answered run into a WallTimeout",
        );
        assert_eq!(
            testval::from_blob(&v.exports),
            TestValue::Number(1.0)
        );
        assert!(!out_a.tainted);
    }

    #[test]
    fn a_terminate_that_races_ahead_of_its_job_still_aborts_the_run() {
        // On the shared event channel a Terminate can be picked up before
        // the run's job is dispatched (select order between the two queues
        // is arbitrary). The loop must remember it and answer the job on
        // arrival — a per-run channel used to hold such events implicitly.
        sandbox::init_platform();
        let (mut server, client) = UnixStream::pair().unwrap();
        let sink = crate::ipc::FrameSink::Shared(crate::ipc::Outbox::spawn(&client).unwrap());
        let handle = spawn_instance(interleave_prefix(), 0, test_brand_key()).expect("spawn instance");
        let counter = Arc::new(AtomicU32::new(0));

        // Park the loop on a first run so it is live and selecting.
        let first = submit_session_job(
            &handle, 31, "viaTool", vec![TestValue::Number(1.0)],
            sink.clone(), &counter, sandbox::Limits::default(),
        );
        let (_, c1) = read_bridge_call(&mut server);

        // The Terminate for a run whose job has NOT been sent yet: the loop
        // is parked with an empty jobs queue, so this event is processed
        // first, deterministically.
        let token = sandbox::alloc_run_token();
        handle
            .event_sender()
            .send(sandbox::RoutedEvent::new(
                token,
                sandbox::RunEvent::Frame(crate::ipc::TypedFrame {
                    message_type: crate::ipc::TsToRustMessageType::Terminate,
                    payload: 32u32.to_be_bytes().to_vec(),
                }),
            ))
            .unwrap();
        std::thread::sleep(Duration::from_millis(50));

        // Now the job arrives — it must conclude Aborted without running
        // (the counter export would observe a bump otherwise).
        let (otx, orx) = crossbeam_channel::bounded(1);
        let job = Box::new(CallJob {
            token,
            code: None,
            filename: None,
            limits: sandbox::Limits::default(),
            globals: vec![crate::ipc::HostGlobalDef::bridge("tool")],
            io: sandbox::RunIo::Instance { sink: sink.clone() },
            call_id_counter: Arc::clone(&counter),
            call: Some(crate::ipc::CallSpec {
                export_path: "bump".to_string(),
                args_blob: testval::to_blob(&TestValue::Array(vec![])),
            }),
            epilogue: Some(sandbox::EpilogueSpec {
                run_id: 32,
                report_heap: false,
            }),
            complete: None,
            ctl_slot: None,
        });
        handle.sender().send((job, Some(otx))).unwrap();
        let out = orx.recv_timeout(Duration::from_secs(5)).expect("job answered");
        assert!(!out.tainted, "a pre-start abandon must not taint");
        assert!(matches!(
            out.result.unwrap_err().error,
            sandbox::RunError::Aborted
        ));

        // The instance is healthy and the aborted run never executed: bump
        // starts from the untouched counter (first's hangs never bumped).
        first.send(bridge_response_event(31, c1, 10.0));
        let out_first = first.outcome.recv().expect("first run completes");
        assert!(!out_first.tainted);
        let outcome = handle.call(bump_job());
        assert!(!outcome.tainted);
        assert_eq!(bump_value(&outcome), 1.0, "the abandoned run never ran bump");
    }

    #[test]
    fn dead_instance_thread_reports_a_tainted_internal_failure() {
        sandbox::init_platform();
        // A prefix that cannot warm up: the owner thread responds with the
        // warm-up failure and exits.
        let looping = Arc::new(PrefixData {
            code: "for (;;) {}".to_string(),
            filename: None,
            globals: Vec::new(),
            declared_globals: Vec::new(),
            declared_imports: Vec::new(),
        });
        let dead = spawn_instance(looping, 0, test_brand_key()).expect("spawn instance");
        let first = dead.call(bump_job());
        assert!(first.tainted);
        assert!(matches!(
            first.result.unwrap_err().error,
            sandbox::RunError::WarmupLimit
        ));
        // The thread exited after the warm-up failure — a second call on the
        // same handle reports the dead-instance outcome instead of hanging.
        let second = dead.call(bump_job());
        assert!(second.tainted);
        assert!(matches!(
            second.result.unwrap_err().error,
            sandbox::RunError::Internal(_)
        ));
    }
}
