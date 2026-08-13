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
//! Capacity model (v2, #65): one global ledger of live isolates — idle warm
//! + busy warm + running one-off — capped at `max_live`, which the host
//! derives from its memory budget (`budget ÷ memoryMb`, floored at the pool
//! size so the pool can always run `maxIsolates` concurrent isolates).
//! Taking a slot when all are held evicts the least-recently-used *idle*
//! instance; running instances are never evicted. The host pool admits at
//! most `maxIsolates` concurrent runs and `max_live >= maxIsolates`, so
//! whenever the cap is hit at least one held slot is idle. Smarter victim
//! selection (`heapUsed × idleTime`) and memory watermarks are #66; the
//! wait-vs-cold-start acquire policy is #77.
//!
//! Instances of one prefix share no state with each other; state inside one
//! instance survives between calls as a cache, never a guarantee — any
//! instance may be evicted at any moment (taint, LRU, dispose).

use std::collections::HashMap;
use std::os::unix::io::RawFd;
use std::sync::atomic::AtomicU32;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::ipc;
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
    /// When this instance last finished a call — LRU eviction key.
    last_used: Instant,
    /// `used_heap_size` after the last call — Result-frame report today,
    /// eviction scoring input in #66.
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
    // Channel disconnected — the registry evicted this instance (taint, LRU,
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
    /// The live-isolate cap this registry enforces.
    pub max_live: usize,
    /// Per-prefix `(prefix_id, idle, busy)` instance counts.
    pub per_prefix: Vec<(String, usize, usize)>,
}

pub struct WarmRegistry {
    inner: Mutex<RegistryInner>,
    /// Global live-isolate cap — host-derived from the memory budget
    /// (`--max-live-isolates`, floored at the pool size). Counts idle warm
    /// + busy warm + running one-off.
    max_live: usize,
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
}

impl RegistryInner {
    /// Evict at least enough idle instances to admit one more live isolate,
    /// or warn and give up when nothing is idle (more concurrent runs than
    /// the cap, which the host pool is supposed to prevent — run over cap
    /// rather than deadlock; the excess corrects itself on release).
    fn make_room(&mut self, max_live: usize) {
        while self.total >= max_live {
            if !evict_lru(self) {
                eprintln!(
                    "[iso4-v8] warm registry over capacity with no idle victim \
                     (total={}, max_live={}) — proceeding over cap",
                    self.total, max_live
                );
                break;
            }
        }
    }
}

impl WarmRegistry {
    pub fn new(max_live: usize) -> Self {
        Self {
            inner: Mutex::new(RegistryInner {
                prefixes: HashMap::new(),
                total: 0,
                oneoff_running: 0,
            }),
            max_live: max_live.max(1),
        }
    }

    /// Take a slot for a run of `prefix_id`: reuse the warmest idle instance
    /// of that prefix, or reserve a slot for a fresh one — evicting the
    /// least-recently-used idle instance (any prefix) when the cap is full.
    /// Running instances are never evicted: the host pool admits at most
    /// `maxIsolates <= max_live` concurrent runs, so a full cap always
    /// contains an idle victim.
    pub fn acquire(&self, prefix_id: &str) -> Acquired {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(slots) = inner.prefixes.get_mut(prefix_id) {
            if let Some(handle) = slots.idle.pop() {
                // total unchanged: the slot moves from idle to busy.
                slots.busy += 1;
                return Acquired::Reused(handle);
            }
        }
        inner.make_room(self.max_live);
        inner.prefixes.entry(prefix_id.to_string()).or_default().busy += 1;
        inner.total += 1;
        Acquired::CreateNew
    }

    /// Same slot accounting for a one-off run (fresh isolate, never reused):
    /// reserve a slot, evicting an idle instance if the cap demands it.
    pub fn reserve_oneoff(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        inner.make_room(self.max_live);
        inner.total += 1;
        inner.oneoff_running += 1;
    }

    /// A one-off run finished; its isolate is already gone.
    pub fn release_oneoff(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        inner.total = inner.total.saturating_sub(1);
        inner.oneoff_running = inner.oneoff_running.saturating_sub(1);
    }

    /// Return a warm instance after a call. Tainted instances and instances
    /// of a disposed prefix are dropped (their owner thread exits and
    /// disposes the isolate); clean instances go back to the idle pool with
    /// fresh LRU/heap metadata.
    pub fn release(
        &self,
        prefix_id: &str,
        mut handle: InstanceHandle,
        outcome_tainted: bool,
        heap_used_bytes: u64,
        prefix_alive: bool,
    ) {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let slots = inner.prefixes.entry(prefix_id.to_string()).or_default();
        slots.busy = slots.busy.saturating_sub(1);
        if outcome_tainted || !prefix_alive {
            if slots.idle.is_empty() && slots.busy == 0 {
                inner.prefixes.remove(prefix_id);
            }
            inner.total = inner.total.saturating_sub(1);
            // Dropping the handle disconnects the job channel; the owner
            // thread exits and the isolate dies on its creating thread.
            return;
        }
        handle.last_used = Instant::now();
        handle.heap_used_bytes = heap_used_bytes;
        slots.idle.push(handle);
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
        RegistryStats {
            oneoff_running: inner.oneoff_running,
            warm_busy,
            warm_idle,
            idle_heap_bytes,
            max_live: self.max_live,
            per_prefix,
        }
    }
}

/// Drop the least-recently-used idle instance across all prefixes. Returns
/// false when nothing is idle.
fn evict_lru(inner: &mut RegistryInner) -> bool {
    let victim = inner
        .prefixes
        .iter()
        .filter(|(_, slots)| !slots.idle.is_empty())
        .min_by_key(|(_, slots)| slots.idle[0].last_used)
        .map(|(id, _)| id.clone());
    let Some(prefix_id) = victim else {
        return false;
    };
    let slots = inner
        .prefixes
        .get_mut(&prefix_id)
        .expect("victim key came from the map");
    // Front of the Vec is the least recently used (reuse pushes/pops the
    // back), so evict index 0.
    let _evicted = slots.idle.remove(0);
    if slots.idle.is_empty() && slots.busy == 0 {
        inner.prefixes.remove(&prefix_id);
    }
    inner.total = inner.total.saturating_sub(1);
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
        let registry = WarmRegistry::new(4);
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
        let registry = WarmRegistry::new(4);
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
    fn cap_evicts_the_least_recently_used_idle_instance() {
        // Accounting-only test: instances are spawned but never called, so no
        // isolates are created (the owner thread builds its core lazily).
        let registry = WarmRegistry::new(1);
        assert!(matches!(registry.acquire("a"), Acquired::CreateNew));
        registry.release("a", spawn_instance(counter_prefix(), 0), false, 0, true);

        // Cap is 1 and the only slot holds a's idle instance — acquiring b
        // must evict it and create fresh.
        assert!(matches!(registry.acquire("b"), Acquired::CreateNew));
        registry.release("b", spawn_instance(counter_prefix(), 0), false, 0, true);

        // And a is gone: acquiring it again must create, evicting b.
        assert!(matches!(registry.acquire("a"), Acquired::CreateNew));
    }

    #[test]
    fn dispose_prefix_drops_idle_instances() {
        let registry = WarmRegistry::new(4);
        assert!(matches!(registry.acquire("p0"), Acquired::CreateNew));
        registry.release("p0", spawn_instance(counter_prefix(), 0), false, 0, true);
        registry.dispose_prefix("p0");
        assert!(matches!(registry.acquire("p0"), Acquired::CreateNew));
    }

    #[test]
    fn release_of_a_disposed_prefix_drops_the_instance() {
        let registry = WarmRegistry::new(4);
        assert!(matches!(registry.acquire("p0"), Acquired::CreateNew));
        // prefix_alive = false: DisposePrefix landed while the call ran.
        registry.release("p0", spawn_instance(counter_prefix(), 0), false, 0, false);
        assert!(matches!(registry.acquire("p0"), Acquired::CreateNew));
    }

    #[test]
    fn one_off_reservation_shares_the_slot_budget() {
        let registry = WarmRegistry::new(1);
        assert!(matches!(registry.acquire("a"), Acquired::CreateNew));
        registry.release("a", spawn_instance(counter_prefix(), 0), false, 0, true);

        // A one-off run at the cap evicts a's idle instance.
        registry.reserve_oneoff();
        registry.release_oneoff();
        assert!(matches!(registry.acquire("a"), Acquired::CreateNew));
    }

    #[test]
    fn stats_snapshot_counts_idle_busy_and_oneoff() {
        let registry = WarmRegistry::new(8);
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
        assert_eq!(stats.max_live, 8);
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
    fn max_live_admits_more_idle_instances_than_the_pool_size() {
        // The #65 point: the live cap comes from the memory budget, so idle
        // warm instances can outnumber concurrent runs. Cap 3, one busy slot
        // at a time: two prefixes stay resident with no eviction.
        let registry = WarmRegistry::new(3);
        assert!(matches!(registry.acquire("a"), Acquired::CreateNew));
        registry.release("a", spawn_instance(counter_prefix(), 0), false, 0, true);
        assert!(matches!(registry.acquire("b"), Acquired::CreateNew));
        registry.release("b", spawn_instance(counter_prefix(), 0), false, 0, true);

        // Both survive: reuse hits for each.
        assert!(matches!(registry.acquire("a"), Acquired::Reused(_)));
        assert!(matches!(registry.acquire("b"), Acquired::Reused(_)));
    }

    #[test]
    fn stats_stay_consistent_after_an_eviction() {
        let registry = WarmRegistry::new(1);
        assert!(matches!(registry.acquire("a"), Acquired::CreateNew));
        registry.release("a", spawn_instance(counter_prefix(), 0), false, 500, true);

        // Cap 1: acquiring "b" evicts a's idle instance — every count and
        // the summed idle heap must reflect that immediately.
        assert!(matches!(registry.acquire("b"), Acquired::CreateNew));
        let stats = registry.stats();
        assert_eq!(stats.warm_idle, 0);
        assert_eq!(stats.idle_heap_bytes, 0);
        assert_eq!(stats.warm_busy, 1);
        assert_eq!(stats.per_prefix, vec![("b".to_string(), 0, 1)]);

        registry.release("b", spawn_instance(counter_prefix(), 0), false, 300, true);
        let stats = registry.stats();
        assert_eq!(stats.warm_idle, 1);
        assert_eq!(stats.warm_busy, 0);
        assert_eq!(stats.idle_heap_bytes, 300);
    }

    #[test]
    fn dispose_prefix_with_a_busy_instance_keeps_its_accounting() {
        let registry = WarmRegistry::new(4);
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
