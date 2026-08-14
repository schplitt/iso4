//! Warm instance registry (#64): prefix id → pool of resident isolates.
//!
//! Every prepared prefix is served by warm instances. An instance is one
//! isolate with the prefix already evaluated, owned cradle-to-grave by a
//! dedicated OS thread — rusty_v8 pins isolates to their creating thread
//! (`OwnedIsolate` is `!Send`, `Drop` asserts current-thread ownership, no
//! `Locker` is exposed), so the owner thread is the only place the isolate
//! can be created, called into, and dropped. Session threads talk to it over
//! a channel; while a call runs, the session thread blocks on the response,
//! so the session socket has exactly one user at a time (the instance thread
//! does the bridge I/O during the call, the session thread between calls —
//! the same discipline as before #64, just on another thread).
//!
//! Capacity model (v3, #66 — celld's, whole): the memory control is ONE
//! RSS mark, the warm budget (`--warm-budget-bytes`, 0 = disabled). Every
//! acquire/release samples the process RSS (~0.4 µs) and folds it through
//! the pure policy in `policy.rs`: RSS at/above the budget latches
//! shedding — evict idle instances by `heapUsed × idleTime` score AND
//! stop pooling NEW instances (a PrefixRun without an idle instance runs
//! on a cold one-off isolate; reuse of already-warm instances stays
//! allowed, it adds no memory) — until RSS falls back to 4/5 of the
//! budget (hysteresis — no flapping). Running instances are never
//! evicted, and correctness never depends on warmth. There is
//! deliberately NO instance-count cap: celld defaults its resident
//! ceiling to `usize::MAX` after a default count cap caused eviction
//! churn; concurrency is bounded by the host pool, memory by the mark.
//! The wait-vs-cold-start acquire policy is #77.
//!
//! Instances of one prefix share no state with each other; state inside one
//! instance survives between calls as a cache, never a guarantee — any
//! instance may be evicted at any moment (taint, scored eviction, dispose).

use std::collections::HashMap;
use std::os::unix::io::RawFd;
use std::sync::atomic::AtomicU32;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::ipc;
use crate::policy;
use crate::rss;
use crate::session::PrefixData;
use crate::v8 as sandbox;

/// One call forwarded to an instance owner thread.
pub struct CallJob {
    /// Postfix source (`execute`) — `None` for a call-only run.
    pub code: Option<String>,
    pub filename: Option<String>,
    pub limits: sandbox::Limits,
    /// Bridge-stub defs for this call (re-installed per call).
    pub globals: Vec<ipc::HostGlobalDef>,
    /// Session socket for bridge I/O during this call. The session thread is
    /// parked on the response channel for the duration, so the fd has one
    /// user at a time.
    pub stream_fd: Option<RawFd>,
    /// The connection's monotonically increasing bridge call-ID counter.
    pub call_id_counter: Arc<AtomicU32>,
    pub call: Option<ipc::CallSpec>,
}

enum Job {
    Call(Box<CallJob>, crossbeam_channel::Sender<sandbox::CallOutcome>),
    // Shutdown is signalled by dropping the sender (channel disconnect);
    // no explicit variant needed.
}

/// A live warm instance as the registry sees it: the channel to its owner
/// thread plus the metadata eviction needs. Dropping the handle disconnects
/// the channel; the owner thread then exits and disposes the isolate on the
/// thread that created it.
pub struct InstanceHandle {
    jobs: crossbeam_channel::Sender<Job>,
    /// When this instance last finished a call — the idleTime factor of the
    /// eviction score (#66).
    last_used: Instant,
    /// `used_heap_size` after the last call — Result-frame report and the
    /// heap factor of the eviction score (#66).
    pub heap_used_bytes: u64,
}

impl InstanceHandle {
    /// Run one call on this instance and wait for the outcome. A dead owner
    /// thread (panic during the call) reports an internal failure with
    /// `tainted: true`, so the caller evicts it like any other taint.
    pub fn call(&self, job: CallJob) -> sandbox::CallOutcome {
        let (tx, rx) = crossbeam_channel::bounded(1);
        if self.jobs.send(Job::Call(Box::new(job), tx)).is_err() {
            return dead_instance_outcome();
        }
        rx.recv().unwrap_or_else(|_| dead_instance_outcome())
    }
}

fn dead_instance_outcome() -> sandbox::CallOutcome {
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
pub fn spawn_instance(prefix: Arc<PrefixData>, memory_mb: u32) -> InstanceHandle {
    let (tx, rx) = crossbeam_channel::unbounded::<Job>();
    std::thread::Builder::new()
        .name("iso4-warm-instance".to_string())
        .spawn(move || instance_main(prefix, memory_mb, rx))
        .expect("failed to spawn warm instance thread");
    InstanceHandle {
        jobs: tx,
        last_used: Instant::now(),
        heap_used_bytes: 0,
    }
}

fn instance_main(
    prefix: Arc<PrefixData>,
    memory_mb: u32,
    jobs: crossbeam_channel::Receiver<Job>,
) {
    let mut core: Option<sandbox::InstanceCore> = None;
    while let Ok(Job::Call(job, respond)) = jobs.recv() {
        if core.is_none() {
            let spec = sandbox::PrefixSpec {
                code: &prefix.code,
                filename: prefix.filename.as_deref().unwrap_or("<prefix>"),
                globals: &prefix.globals,
            };
            match sandbox::create_instance_core(Some(spec), &prefix.declared_imports, memory_mb) {
                Ok(c) => core = Some(c),
                Err(f) => {
                    // Warm-up failed — report it as this call's outcome and
                    // exit; the registry drops the handle on taint.
                    respond
                        .send(sandbox::CallOutcome {
                            result: Err(f),
                            tainted: true,
                            heap_used_bytes: 0,
                        })
                        .ok();
                    return;
                }
            }
        }
        let outcome = sandbox::run_call_on_core(
            core.as_mut().expect("core created above"),
            job.code.as_deref(),
            job.filename.as_deref().unwrap_or("<iso4>"),
            &prefix.globals,
            job.limits,
            &job.globals,
            &prefix.declared_imports,
            job.stream_fd,
            job.call_id_counter,
            job.call.as_ref(),
        );
        respond.send(outcome).ok();
    }
    // Channel disconnected — the registry evicted this instance (taint, eviction,
    // dispose) or the process is shutting down. `core` drops here, on the
    // thread that created the isolate, as rusty_v8 requires.
}

/// What `acquire` hands the session thread.
pub enum Acquired {
    /// An idle instance of this prefix — use it, then `release` it.
    Reused(InstanceHandle),
    /// No idle instance; a slot has been reserved (evicting an idle victim
    /// if the cap demanded it). Spawn a fresh instance, then `release` it.
    CreateNew,
    /// Shedding: RSS is at/over the warm budget (or still above the
    /// release line), so no new instance may be pooled. Spawn a fresh
    /// instance, run the call, then DROP the handle and call
    /// `release_oneoff` — never `release`. Accounted through the one-off
    /// counters (same semantics: fresh isolate, never reused).
    CreateCold,
}

/// A point-in-time snapshot of the registry for `stats()` (#65). Counts are
/// consistent with each other (taken under one lock), stale the moment the
/// lock drops — diagnostics, not synchronization.
pub struct RegistryStats {
    /// Running one-off isolates (`sandbox.run()`).
    pub oneoff_running: usize,
    /// Warm instances currently serving a call.
    pub warm_busy: usize,
    /// Idle warm instances ready for reuse.
    pub warm_idle: usize,
    /// Summed `heap_used_bytes` of the idle instances (last-call
    /// measurements; busy instances' current heap is unknown mid-call).
    pub idle_heap_bytes: u64,
    /// Per-prefix `(prefix_id, idle, busy)` instance counts.
    pub per_prefix: Vec<(String, usize, usize)>,
    /// The warm budget this registry sheds against (0 = disabled) — next
    /// to `rss_bytes` so utilization is computable from one snapshot.
    pub warm_budget_bytes: u64,
    /// The runtime process's RSS at snapshot time (0 when unreadable) —
    /// the signal the mark acts on (#66).
    pub rss_bytes: u64,
    /// The shedding latch: RSS reached the budget and has not yet fallen
    /// back to 4/5 of it.
    pub under_pressure: bool,
}

pub struct WarmRegistry {
    inner: Mutex<RegistryInner>,
    /// The warm budget in bytes (`--warm-budget-bytes`), the one RSS mark.
    /// 0 disables the watermarks — nothing bounds warmth then except
    /// `dispose()`; the host's default budget makes that an explicit
    /// opt-out, not a default.
    warm_budget_bytes: u64,
    /// Test seam: watermark tests inject an RSS sample instead of reading
    /// the real process (u64::MAX = unset). Never set outside tests.
    #[cfg(test)]
    rss_override: std::sync::atomic::AtomicU64,
}

/// Instances of one prefix as the registry tracks them.
#[derive(Default)]
struct PrefixSlots {
    /// Idle instances, most-recently-used at the back (reuse pops the
    /// warmest; eviction removes the front).
    idle: Vec<InstanceHandle>,
    /// Instances currently serving a call — counted for `stats()` and for
    /// the per-prefix state #77's acquire policy will need.
    busy: usize,
}

struct RegistryInner {
    /// Per-prefix instance state. An entry exists while the prefix has any
    /// instance (idle or busy) and is removed when both drain to zero.
    prefixes: HashMap<String, PrefixSlots>,
    /// Live isolates: idle warm + busy warm + running one-off.
    total: usize,
    /// Running one-off isolates — stats only; they also count in `total`.
    oneoff_running: usize,
    /// Idle instances across all prefixes — maintained (not recomputed) so
    /// the per-event pressure check stays O(1) when nothing is shed.
    /// `stats()` cross-checks it against the recomputed sum.
    idle_total: usize,
    /// The shedding latch: fed back into the next watermark verdict as
    /// `was_shedding` (hysteresis).
    shedding: bool,
    /// The last completed shed pass — the futility check compares the next
    /// RSS sample against it. Cleared when the latch releases.
    last_pass: Option<policy::PassOutcome>,
}

impl RegistryInner {
    /// Fold one RSS sample through the watermark policy and perform the
    /// verdict: update the latch, shed the pass's worth of victims, record
    /// the pass for the futility check. Returns whether a NEW instance may
    /// be pooled (`false` while shedding — celld couples the admission
    /// stop to the latch). `None` (no reading available) changes nothing
    /// and admits — watermarks unavailable is not pressure.
    fn pressure_pass(&mut self, rss_sample: Option<u64>, budget_bytes: u64) -> bool {
        let Some(rss_bytes) = rss_sample else {
            return true;
        };
        let verdict = policy::watermark_action(&policy::PressureFacts {
            rss_bytes,
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
                    rss_at_pass: rss_bytes,
                });
            }
        }
        !verdict.shedding
    }
}

impl WarmRegistry {
    pub fn new(warm_budget_bytes: u64) -> Self {
        Self {
            inner: Mutex::new(RegistryInner {
                prefixes: HashMap::new(),
                total: 0,
                oneoff_running: 0,
                idle_total: 0,
                shedding: false,
                last_pass: None,
            }),
            warm_budget_bytes,
            #[cfg(test)]
            rss_override: std::sync::atomic::AtomicU64::new(u64::MAX),
        }
    }

    /// One RSS sample for the pressure check — taken BEFORE the registry
    /// lock (a syscall does not belong under the mutex). Skipped entirely
    /// when watermarks are disabled, so the disabled path pays nothing.
    fn sample_rss(&self) -> Option<u64> {
        if self.warm_budget_bytes == 0 {
            return None;
        }
        #[cfg(test)]
        {
            let injected = self.rss_override.load(std::sync::atomic::Ordering::Relaxed);
            if injected != u64::MAX {
                return Some(injected);
            }
        }
        rss::process_rss_bytes()
    }

    /// Watermark tests inject the RSS sample; everything downstream of the
    /// number is deterministic (the point of the pure policy layer).
    #[cfg(test)]
    fn set_rss_for_test(&self, rss_bytes: u64) {
        self.rss_override
            .store(rss_bytes, std::sync::atomic::Ordering::Relaxed);
    }

    /// Take a slot for a run of `prefix_id`: reuse the warmest idle instance
    /// of that prefix, or create a fresh one — shedding scored victims
    /// first when the RSS mark demands it, and degrading to a cold one-off
    /// (`CreateCold`) while the shedding latch is held. Running instances
    /// are never evicted; concurrency is bounded by the host pool, not
    /// here.
    pub fn acquire(&self, prefix_id: &str) -> Acquired {
        let rss_sample = self.sample_rss();
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let admit_warm = inner.pressure_pass(rss_sample, self.warm_budget_bytes);
        if let Some(slots) = inner.prefixes.get_mut(prefix_id) {
            if let Some(handle) = slots.idle.pop() {
                // total unchanged: the slot moves from idle to busy. Reuse
                // stays allowed while shedding — an existing instance adds
                // no memory.
                slots.busy += 1;
                inner.idle_total -= 1;
                return Acquired::Reused(handle);
            }
        }
        if !admit_warm {
            // Shedding and nothing warm to reuse: fresh isolate for this
            // call only, accounted as a one-off (it never joins a pool).
            inner.total += 1;
            inner.oneoff_running += 1;
            return Acquired::CreateCold;
        }
        inner.prefixes.entry(prefix_id.to_string()).or_default().busy += 1;
        inner.total += 1;
        Acquired::CreateNew
    }

    /// Same ledger accounting for a one-off run (fresh isolate, never
    /// reused). Never refused: transient work gives its memory back on its
    /// own (celld keeps stateless admission open under pressure for the
    /// same reason).
    pub fn reserve_oneoff(&self) {
        let rss_sample = self.sample_rss();
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        inner.pressure_pass(rss_sample, self.warm_budget_bytes);
        inner.total += 1;
        inner.oneoff_running += 1;
    }

    /// A one-off run finished; its isolate is already gone. Also the
    /// release path for `CreateCold` instances (the session drops the
    /// handle instead of pooling it).
    pub fn release_oneoff(&self) {
        let rss_sample = self.sample_rss();
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        inner.total = inner.total.saturating_sub(1);
        inner.oneoff_running = inner.oneoff_running.saturating_sub(1);
        inner.pressure_pass(rss_sample, self.warm_budget_bytes);
    }

    /// Return a warm instance after a call. Tainted instances and instances
    /// of a disposed prefix are dropped (their owner thread exits and
    /// disposes the isolate); clean instances go back to the idle pool with
    /// fresh last-used/heap metadata (the eviction-score inputs).
    pub fn release(
        &self,
        prefix_id: &str,
        mut handle: InstanceHandle,
        outcome_tainted: bool,
        heap_used_bytes: u64,
        prefix_alive: bool,
    ) {
        let rss_sample = self.sample_rss();
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let slots = inner.prefixes.entry(prefix_id.to_string()).or_default();
        slots.busy = slots.busy.saturating_sub(1);
        if outcome_tainted || !prefix_alive {
            if slots.idle.is_empty() && slots.busy == 0 {
                inner.prefixes.remove(prefix_id);
            }
            inner.total = inner.total.saturating_sub(1);
            inner.pressure_pass(rss_sample, self.warm_budget_bytes);
            // Dropping the handle disconnects the job channel; the owner
            // thread exits and the isolate dies on its creating thread.
            return;
        }
        handle.last_used = Instant::now();
        handle.heap_used_bytes = heap_used_bytes;
        slots.idle.push(handle);
        inner.idle_total += 1;
        // Pressure check AFTER pooling: the just-released instance is a
        // candidate like any other (its score is ~0 — no grace period
        // needed, the idleTime factor already protects it; see #66).
        inner.pressure_pass(rss_sample, self.warm_budget_bytes);
    }

    /// Drop every idle instance of a disposed prefix. Busy instances are
    /// handled at `release` time via `prefix_alive: false` (the entry stays
    /// until their release drains `busy` to zero).
    pub fn dispose_prefix(&self, prefix_id: &str) {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let Some(slots) = inner.prefixes.get_mut(prefix_id) else {
            return;
        };
        let dropped = slots.idle.len();
        slots.idle.clear(); // Handles drop here → owner threads exit.
        let still_busy = slots.busy > 0;
        inner.total = inner.total.saturating_sub(dropped);
        inner.idle_total = inner.idle_total.saturating_sub(dropped);
        if !still_busy {
            inner.prefixes.remove(prefix_id);
        }
    }

    /// Snapshot the registry for the `Stats` frame (#65).
    pub fn stats(&self) -> RegistryStats {
        let inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let mut warm_busy = 0;
        let mut warm_idle = 0;
        let mut idle_heap_bytes = 0u64;
        let mut per_prefix: Vec<(String, usize, usize)> = inner
            .prefixes
            .iter()
            .map(|(id, slots)| {
                warm_busy += slots.busy;
                warm_idle += slots.idle.len();
                idle_heap_bytes += slots.idle.iter().map(|h| h.heap_used_bytes).sum::<u64>();
                (id.clone(), slots.idle.len(), slots.busy)
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
            warm_idle,
            idle_heap_bytes,
            per_prefix,
            warm_budget_bytes: self.warm_budget_bytes,
            // Read directly (not `sample_rss`): stats are diagnostics and
            // want the number even when watermarks are disabled.
            rss_bytes: rss::process_rss_bytes().unwrap_or(0),
            under_pressure: inner.shedding,
        }
    }
}

/// Drop the highest-scored idle instance across ALL prefixes (#66):
/// `heapUsed × idleTime`, ties to the longest-idle — `policy::pick_victim`
/// decides, this function only gathers facts and performs. Returns false
/// when nothing is idle. The single victim-picking path: every eviction
/// (shed passes today, anything later) comes through here, so policies
/// can never disagree about what may be taken.
fn evict_scored(inner: &mut RegistryInner, now: Instant) -> bool {
    let mut facts: Vec<policy::VictimFact> = Vec::with_capacity(inner.idle_total);
    let mut locations: Vec<(&String, usize)> = Vec::with_capacity(inner.idle_total);
    for (prefix_id, slots) in &inner.prefixes {
        for (index, handle) in slots.idle.iter().enumerate() {
            facts.push(policy::VictimFact {
                heap_used_bytes: handle.heap_used_bytes,
                last_used: handle.last_used,
            });
            locations.push((prefix_id, index));
        }
    }
    let Some(pick) = policy::pick_victim(&facts, now) else {
        return false;
    };
    let (prefix_id, index) = (locations[pick].0.clone(), locations[pick].1);
    let slots = inner
        .prefixes
        .get_mut(&prefix_id)
        .expect("victim key came from the map");
    let _evicted = slots.idle.remove(index);
    if slots.idle.is_empty() && slots.busy == 0 {
        inner.prefixes.remove(&prefix_id);
    }
    inner.total = inner.total.saturating_sub(1);
    inner.idle_total = inner.idle_total.saturating_sub(1);
    true
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testval::{self, TestValue};

    fn counter_prefix() -> Arc<PrefixData> {
        Arc::new(PrefixData {
            code: "let n = 0\nexport function bump() { return ++n }".to_string(),
            filename: None,
            globals: Vec::new(),
            declared_globals: Vec::new(),
            declared_imports: Vec::new(),
        })
    }

    fn bump_job() -> CallJob {
        CallJob {
            code: None,
            filename: None,
            limits: sandbox::Limits::default(),
            globals: Vec::new(),
            stream_fd: None,
            call_id_counter: Arc::new(AtomicU32::new(0)),
            call: Some(ipc::CallSpec {
                export_path: "bump".to_string(),
                args_blob: testval::to_blob(&TestValue::Array(Vec::new())),
            }),
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
        let handle = spawn_instance(counter_prefix(), 0);
        for expected in 1..=3 {
            let outcome = handle.call(bump_job());
            assert!(!outcome.tainted);
            assert_eq!(bump_value(&outcome), f64::from(expected));
        }
    }

    #[test]
    fn registry_reuses_a_released_instance() {
        sandbox::init_platform();
        let registry = WarmRegistry::new(0);
        let Acquired::CreateNew = registry.acquire("p0") else {
            panic!("first acquire must create");
        };
        let handle = spawn_instance(counter_prefix(), 0);
        let outcome = handle.call(bump_job());
        assert_eq!(bump_value(&outcome), 1.0);
        registry.release("p0", handle, false, outcome.heap_used_bytes, true);

        // Same prefix again: the idle instance comes back, state intact.
        let Acquired::Reused(handle) = registry.acquire("p0") else {
            panic!("second acquire must reuse the idle instance");
        };
        let outcome = handle.call(bump_job());
        assert_eq!(bump_value(&outcome), 2.0);
        registry.release("p0", handle, false, outcome.heap_used_bytes, true);
    }

    #[test]
    fn tainted_release_discards_the_instance() {
        sandbox::init_platform();
        let registry = WarmRegistry::new(0);
        assert!(matches!(registry.acquire("p0"), Acquired::CreateNew));
        let handle = spawn_instance(counter_prefix(), 0);
        let outcome = handle.call(bump_job());
        registry.release("p0", handle, true, outcome.heap_used_bytes, true);

        // Nothing idle — the next acquire cold-starts, state reset.
        let Acquired::CreateNew = registry.acquire("p0") else {
            panic!("tainted instance must not be reused");
        };
        let handle = spawn_instance(counter_prefix(), 0);
        let outcome = handle.call(bump_job());
        assert_eq!(bump_value(&outcome), 1.0, "fresh instance, fresh state");
        registry.release("p0", handle, false, outcome.heap_used_bytes, true);
    }

    #[test]
    fn shedding_degenerates_to_lru_with_equal_heaps() {
        // Accounting-only test: instances are spawned but never called, so
        // no isolates are created (the owner thread builds its core lazily).
        // Zero heaps → every score is 0 → the pick falls back to the
        // longest-idle, i.e. plain LRU, exactly the celld order.
        let registry = WarmRegistry::new(100 * TEST_MB);
        registry.set_rss_for_test(10 * TEST_MB);
        assert!(matches!(registry.acquire("a"), Acquired::CreateNew));
        registry.release("a", spawn_instance(counter_prefix(), 0), false, 0, true);
        assert!(matches!(registry.acquire("b"), Acquired::CreateNew));
        registry.release("b", spawn_instance(counter_prefix(), 0), false, 0, true);

        // RSS reaches the mark: the pass sheds one (2 idle / 10 → min 1),
        // and it must be "a" — released first, idle longest.
        registry.set_rss_for_test(100 * TEST_MB);
        registry.reserve_oneoff();
        registry.release_oneoff();
        let stats = registry.stats();
        assert_eq!(stats.warm_idle, 1);
        assert_eq!(stats.per_prefix, vec![("b".to_string(), 1, 0)]);
    }

    #[test]
    fn dispose_prefix_drops_idle_instances() {
        let registry = WarmRegistry::new(0);
        assert!(matches!(registry.acquire("p0"), Acquired::CreateNew));
        registry.release("p0", spawn_instance(counter_prefix(), 0), false, 0, true);
        registry.dispose_prefix("p0");
        assert!(matches!(registry.acquire("p0"), Acquired::CreateNew));
    }

    #[test]
    fn release_of_a_disposed_prefix_drops_the_instance() {
        let registry = WarmRegistry::new(0);
        assert!(matches!(registry.acquire("p0"), Acquired::CreateNew));
        // prefix_alive = false: DisposePrefix landed while the call ran.
        registry.release("p0", spawn_instance(counter_prefix(), 0), false, 0, false);
        assert!(matches!(registry.acquire("p0"), Acquired::CreateNew));
    }

    #[test]
    fn without_a_budget_nothing_is_ever_evicted() {
        // Watermarks disabled (budget 0) and no count cap exists anymore
        // (celld defaults its resident ceiling to usize::MAX): any number
        // of prefixes stays resident, and one-off traffic evicts nothing.
        let registry = WarmRegistry::new(0);
        for prefix in ["a", "b", "c", "d"] {
            assert!(matches!(registry.acquire(prefix), Acquired::CreateNew));
            registry.release(prefix, spawn_instance(counter_prefix(), 0), false, 0, true);
        }
        registry.reserve_oneoff();
        registry.release_oneoff();
        for prefix in ["a", "b", "c", "d"] {
            assert!(matches!(registry.acquire(prefix), Acquired::Reused(_)));
        }
    }

    const TEST_MB: u64 = 1024 * 1024;

    #[test]
    fn shedding_evicts_the_highest_scored_victim() {
        // Budget 100 MB = the mark; release line 80 MB. Two idle instances:
        // "big" released later with a 1 GB heap claim, "small" earlier with
        // 1 KB — the score (heap × idle) must pick "big" despite it being
        // younger. (The heap ratio dwarfs the nanoseconds between the two
        // releases, so the pick cannot flip on test-machine timing.)
        let registry = WarmRegistry::new(100 * TEST_MB);
        registry.set_rss_for_test(10 * TEST_MB);
        assert!(matches!(registry.acquire("small"), Acquired::CreateNew));
        registry.release("small", spawn_instance(counter_prefix(), 0), false, 1_000, true);
        assert!(matches!(registry.acquire("big"), Acquired::CreateNew));
        registry.release("big", spawn_instance(counter_prefix(), 0), false, 1024 * TEST_MB, true);

        registry.set_rss_for_test(100 * TEST_MB);
        // Any registry event runs the pressure pass: idle 2 → shed target 1.
        registry.reserve_oneoff();
        let stats = registry.stats();
        assert!(stats.under_pressure);
        assert_eq!(stats.warm_idle, 1);
        assert_eq!(stats.per_prefix, vec![("small".to_string(), 1, 0)]);

        // Same RSS on the next event: the pass left it flat (the OS returns
        // freed pages lazily) — futility stops the walk, "small" survives.
        registry.release_oneoff();
        assert_eq!(registry.stats().warm_idle, 1);

        // The latch holds between the release line (80 MB) and the mark,
        // and a moved sample re-arms the walk — it takes the last idle
        // instance.
        registry.set_rss_for_test(90 * TEST_MB);
        registry.reserve_oneoff();
        registry.release_oneoff();
        let stats = registry.stats();
        assert!(stats.under_pressure);
        assert_eq!(stats.warm_idle, 0);

        registry.set_rss_for_test(80 * TEST_MB);
        registry.reserve_oneoff();
        registry.release_oneoff();
        assert!(!registry.stats().under_pressure);
    }

    #[test]
    fn shedding_degrades_new_warmth_to_cold_but_reuses_existing() {
        let registry = WarmRegistry::new(100 * TEST_MB);
        registry.set_rss_for_test(10 * TEST_MB);
        assert!(matches!(registry.acquire("p"), Acquired::CreateNew));
        registry.release("p", spawn_instance(counter_prefix(), 0), false, 1_000, true);
        assert!(matches!(registry.acquire("q"), Acquired::CreateNew));
        registry.release("q", spawn_instance(counter_prefix(), 0), false, 1024 * TEST_MB, true);

        // At the mark: one shed pass takes "q" (highest score); the flat
        // sample after it stops the walk, so "p" stays.
        registry.set_rss_for_test(100 * TEST_MB);
        registry.reserve_oneoff();
        registry.release_oneoff();
        assert_eq!(registry.stats().warm_idle, 1);

        // Reuse of existing warmth stays allowed — it adds no memory…
        let Acquired::Reused(handle) = registry.acquire("p") else {
            panic!("existing idle instance must be reused while shedding");
        };
        // …but nothing NEW may be pooled while the latch is held: a prefix
        // without an idle instance degrades to a cold one-off, accounted on
        // the one-off ledger (celld: a pressured node serves on what it
        // has, but must not build another heap).
        assert!(matches!(registry.acquire("q"), Acquired::CreateCold));
        let stats = registry.stats();
        assert_eq!(stats.oneoff_running, 1);
        assert_eq!(stats.warm_busy, 1);
        registry.release_oneoff();
        registry.release("p", handle, false, TEST_MB, true);

        // Latch released (at/below 80 % of the mark): normal pooling.
        registry.set_rss_for_test(50 * TEST_MB);
        assert!(matches!(registry.acquire("r"), Acquired::CreateNew));
    }

    #[test]
    fn cold_accounting_balances_the_ledger() {
        // CreateCold reserves on the one-off counters; release_oneoff must
        // drain both counters back to zero (the session's cold path).
        let registry = WarmRegistry::new(100 * TEST_MB);
        registry.set_rss_for_test(100 * TEST_MB);
        assert!(matches!(registry.acquire("p"), Acquired::CreateCold));
        assert_eq!(registry.stats().oneoff_running, 1);
        registry.release_oneoff();
        let stats = registry.stats();
        assert_eq!(stats.oneoff_running, 0);
        assert_eq!(stats.warm_busy, 0);
        assert_eq!(stats.warm_idle, 0);
        assert!(stats.per_prefix.is_empty());
    }

    #[test]
    fn stats_snapshot_counts_idle_busy_and_oneoff() {
        let registry = WarmRegistry::new(0);
        // Two instances of "a": one stays busy, one is released idle with a
        // known heap measurement. Plus one running one-off.
        assert!(matches!(registry.acquire("a"), Acquired::CreateNew));
        assert!(matches!(registry.acquire("a"), Acquired::CreateNew));
        registry.release("a", spawn_instance(counter_prefix(), 0), false, 1_000, true);
        registry.reserve_oneoff();

        let stats = registry.stats();
        assert_eq!(stats.oneoff_running, 1);
        assert_eq!(stats.warm_busy, 1);
        assert_eq!(stats.warm_idle, 1);
        assert_eq!(stats.idle_heap_bytes, 1_000);
        assert_eq!(stats.warm_budget_bytes, 0);
        assert_eq!(stats.per_prefix, vec![("a".to_string(), 1, 1)]);

        // Drain everything: counts return to zero and the entry disappears.
        registry.release_oneoff();
        let Acquired::Reused(_) = registry.acquire("a") else {
            panic!("idle instance must be reused");
        };
        registry.release("a", spawn_instance(counter_prefix(), 0), true, 0, true);
        registry.release("a", spawn_instance(counter_prefix(), 0), true, 0, true);
        let stats = registry.stats();
        assert_eq!(stats.oneoff_running, 0);
        assert_eq!(stats.warm_busy, 0);
        assert_eq!(stats.warm_idle, 0);
        assert_eq!(stats.idle_heap_bytes, 0);
        assert!(stats.per_prefix.is_empty());
    }

    #[test]
    fn stats_stay_consistent_after_an_eviction() {
        let registry = WarmRegistry::new(100 * TEST_MB);
        registry.set_rss_for_test(10 * TEST_MB);
        assert!(matches!(registry.acquire("a"), Acquired::CreateNew));
        registry.release("a", spawn_instance(counter_prefix(), 0), false, 500, true);

        // RSS reaches the mark: the one-off's pressure pass evicts a's idle
        // instance — every count and the summed idle heap must reflect
        // that immediately.
        registry.set_rss_for_test(100 * TEST_MB);
        registry.reserve_oneoff();
        let stats = registry.stats();
        assert_eq!(stats.warm_idle, 0);
        assert_eq!(stats.idle_heap_bytes, 0);
        assert_eq!(stats.oneoff_running, 1);
        assert!(stats.per_prefix.is_empty());
        registry.release_oneoff();

        // Pressure gone: pooling resumes and the counts follow.
        registry.set_rss_for_test(10 * TEST_MB);
        assert!(matches!(registry.acquire("b"), Acquired::CreateNew));
        registry.release("b", spawn_instance(counter_prefix(), 0), false, 300, true);
        let stats = registry.stats();
        assert_eq!(stats.warm_idle, 1);
        assert_eq!(stats.warm_busy, 0);
        assert_eq!(stats.idle_heap_bytes, 300);
    }

    #[test]
    fn dispose_prefix_with_a_busy_instance_keeps_its_accounting() {
        let registry = WarmRegistry::new(0);
        assert!(matches!(registry.acquire("p0"), Acquired::CreateNew));
        // Dispose lands while the instance is busy: nothing idle to drop yet.
        registry.dispose_prefix("p0");
        let stats = registry.stats();
        assert_eq!(stats.warm_busy, 1);
        // The release then observes the prefix gone and drops the instance.
        registry.release("p0", spawn_instance(counter_prefix(), 0), false, 0, false);
        let stats = registry.stats();
        assert_eq!(stats.warm_busy, 0);
        assert!(stats.per_prefix.is_empty());
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
        let dead = spawn_instance(looping, 0);
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
