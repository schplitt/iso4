//! The limit guard: a turn arms its CPU and wall deadlines on one
//! process-global watchdog thread, which terminates the isolate on expiry.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Once;
use std::sync::OnceLock;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use crate::v8::CpuBudget;
use crate::v8::isolate::{ReasonCell, TerminationReason};

/// What the watchdog enforces while a turn is executing.
pub(super) struct GuardTarget {
    pub(super) budget: Arc<CpuBudget>,
    /// 0 = no CPU cap.
    pub(super) cpu_cap_ms: u32,
    /// Absolute wall deadline, enforced mid-turn too, so a synchronous spin
    /// under `cpuTimeMs: 0` still dies at the wall (E2 ruling closing the
    /// boundary-only gap). `None` = no wall cap this turn.
    pub(super) wall_deadline: Option<std::time::Instant>,
}

/// State one instance shares with the process-global [`Watchdog`]: the armed
/// target, the executing-turn token, and the arm generation its heap entries
/// are validated against.
struct GuardShared {
    /// The armed target. Set at turn entry and cleared at turn exit UNDER
    /// THE LOCK, and the watchdog fires under the same lock, so a fire and
    /// a clear serialize: after `clear()` returns, no fire for that turn
    /// can happen anymore, and any fire that won the race is already
    /// visible in the reason cell. Between turns the target is empty: a
    /// wall deadline passing while a run is parked is the loop's business
    /// (a clean boundary failure), never the watchdog's.
    target: Mutex<Option<GuardTarget>>,
    /// The token of the run whose turn is executing right now (0 = none).
    /// Written under the target lock, so it serializes with turn exit; a hard
    /// Terminate consults it to choose abandon vs mid-turn kill.
    executing: AtomicU64,
    /// Arm generation, bumped under the target lock by every arm and clear.
    /// A watchdog entry fires only when the generation it was pushed under
    /// is still current — deregistration is this bump, nothing is removed
    /// from the heap. Same stale-fire discipline as the per-turn run
    /// tokens, applied to entries that pop after their turn ended.
    generation: AtomicU64,
    handle: v8::IsolateHandle,
    /// First-writer-wins verdict slot, written before terminating.
    reason: Arc<ReasonCell>,
}

/// The per-instance face of the process-global [`Watchdog`] (amending
/// E1 ruling 2's "one guard thread per instance"): arming a turn registers
/// its earliest deadline on the watchdog heap; clearing bumps the arm
/// generation so the registered entry goes stale. An idle instance holds no
/// live entry and costs no wakeups — and no thread: the owner thread is the
/// only thread a warm instance keeps.
pub struct InstanceGuard {
    shared: Arc<GuardShared>,
}

/// The cross-thread face of an [`InstanceGuard`], handed to the session
/// demux so a Terminate for a run that is mid-turn can terminate it.
#[derive(Clone)]
pub struct GuardCtl {
    shared: Arc<GuardShared>,
}

impl GuardCtl {
    /// Terminate execution iff `token`'s turn is executing right now — the
    /// HARD abort's mid-turn kill (soft aborts never call this: they ride
    /// the routed frame to the turn boundary). Serialized with turn exit
    /// under the target lock: a `true` return means the kill landed
    /// mid-turn (the taint path); `false` means the run is between turns
    /// and the routed Terminate frame will abandon it cleanly.
    ///
    /// The reason is recorded BEFORE the terminate so the turn end detects
    /// the kill deterministically — V8's terminate flag alone is unreliable
    /// (a microtask checkpoint consumes it without surfacing anything,
    /// which used to leave the aborted run parked forever).
    pub fn abort_executing(&self, token: u64) -> bool {
        let _lock = self.shared.target.lock().unwrap_or_else(|p| p.into_inner());
        if self.shared.executing.load(Ordering::Relaxed) == token {
            self.shared.reason.set(TerminationReason::Abort);
            self.shared.handle.terminate_execution();
            true
        } else {
            false
        }
    }
}

impl InstanceGuard {
    pub(super) fn new(handle: v8::IsolateHandle, reason: Arc<ReasonCell>) -> Self {
        Self {
            shared: Arc::new(GuardShared {
                target: Mutex::new(None),
                executing: AtomicU64::new(0),
                generation: AtomicU64::new(0),
                handle,
                reason,
            }),
        }
    }

    pub(super) fn handle(&self) -> &v8::IsolateHandle {
        &self.shared.handle
    }

    pub fn ctl(&self) -> GuardCtl {
        GuardCtl {
            shared: Arc::clone(&self.shared),
        }
    }

    pub(super) fn set(&self, t: GuardTarget) {
        self.set_for(t, 0)
    }

    /// Arm the target, mark `token`'s turn as the one executing (0 for
    /// spans that are not a run's turn, e.g. warm-up), and register the
    /// turn's earliest deadline with the watchdog. An uncapped target
    /// registers nothing — it is still armed so `abort_executing` can match
    /// the turn.
    pub(super) fn set_for(&self, t: GuardTarget, token: u64) {
        // The turn's epoch has not opened yet, so this reads the banked total
        // without touching a thread clock.
        let cpu_ms = (t.cpu_cap_ms > 0).then(|| t.budget.elapsed_ms());
        let next_check = next_check_for(&t, std::time::Instant::now(), cpu_ms);
        let generation;
        {
            let mut guard = self.shared.target.lock().unwrap_or_else(|p| p.into_inner());
            self.shared.executing.store(token, Ordering::Relaxed);
            generation = self.shared.generation.fetch_add(1, Ordering::Relaxed) + 1;
            *guard = Some(t);
        }
        if let Some(next_check) = next_check {
            watchdog().register(WatchEntry {
                next_check,
                generation,
                shared: Arc::clone(&self.shared),
            });
        }
    }

    pub(super) fn clear(&self) {
        let mut guard = self.shared.target.lock().unwrap_or_else(|p| p.into_inner());
        self.shared.executing.store(0, Ordering::Relaxed);
        self.shared.generation.fetch_add(1, Ordering::Relaxed);
        *guard = None;
    }
}

impl Drop for InstanceGuard {
    /// Disarm on drop: early-return paths can tear an instance down with
    /// the target still armed (warm-up/validation `?`s), and the watchdog
    /// entry they leave behind must go stale rather than fire against a
    /// disposed isolate. (The terminate itself would be a harmless no-op —
    /// an `IsolateHandle` stays valid past teardown by construction — but
    /// the shared reason cell must not be dirtied.)
    fn drop(&mut self) {
        self.clear();
    }
}

/// One watchdog heap entry: re-read `shared`'s armed budgets at
/// `next_check`. Ordered by deadline only — entries due at the same instant
/// are interchangeable.
struct WatchEntry {
    next_check: std::time::Instant,
    /// The arm generation this entry was pushed under (see [`GuardShared`]).
    generation: u64,
    shared: Arc<GuardShared>,
}

impl PartialEq for WatchEntry {
    fn eq(&self, other: &Self) -> bool {
        self.next_check == other.next_check
    }
}
impl Eq for WatchEntry {}
impl PartialOrd for WatchEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for WatchEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.next_check.cmp(&other.next_check)
    }
}

/// The process-global CPU-budget watchdog: ONE lazily-started thread
/// owning a min-heap of armed turn deadlines, replacing the per-instance
/// guard thread — so a warm instance holds one OS thread instead of two,
/// which doubles warm residency under hard per-container thread quotas.
/// isolated-vm's production model.
///
/// CPU budgets are polled, never slept-to: a [`CpuBudget`] only advances
/// while its turn executes, so the heap holds the earliest *estimated*
/// expiry and each pop re-reads the budget — re-arming when the estimate
/// was optimistic, firing only on a genuinely exceeded cap. The heap's
/// stale tail (lazily deregistered entries) is bounded by the arm rate
/// times the deadline horizon; every entry costs one pop plus one atomic
/// load to discard.
struct Watchdog {
    heap: Mutex<BinaryHeap<Reverse<WatchEntry>>>,
    /// Wakes the thread when a pushed deadline preempts the current one.
    wake: Condvar,
}

/// The process-global watchdog, started on the first armed deadline.
fn watchdog() -> &'static Watchdog {
    static WATCHDOG: OnceLock<Watchdog> = OnceLock::new();
    static START: Once = Once::new();
    let wd = WATCHDOG.get_or_init(|| Watchdog {
        heap: Mutex::new(BinaryHeap::new()),
        wake: Condvar::new(),
    });
    START.call_once(|| {
        std::thread::Builder::new()
            .name("iso4-watchdog".to_string())
            .spawn(move || wd.run())
            .expect("failed to spawn watchdog thread");
    });
    wd
}

impl Watchdog {
    fn register(&self, entry: WatchEntry) {
        let mut heap = self.heap.lock().unwrap_or_else(|p| p.into_inner());
        let preempts = heap
            .peek()
            .is_none_or(|Reverse(top)| entry.next_check < top.next_check);
        heap.push(Reverse(entry));
        if preempts {
            self.wake.notify_one();
        }
    }

    fn run(&self) {
        let mut heap = self.heap.lock().unwrap_or_else(|p| p.into_inner());
        loop {
            let due = match heap.peek() {
                None => {
                    heap = self.wake.wait(heap).unwrap_or_else(|p| p.into_inner());
                    continue;
                }
                Some(Reverse(top)) => {
                    let now = std::time::Instant::now();
                    if top.next_check > now {
                        let wait = top.next_check - now;
                        heap = self
                            .wake
                            .wait_timeout(heap, wait)
                            .unwrap_or_else(|p| p.into_inner())
                            .0;
                        continue;
                    }
                    heap.pop().expect("peeked above").0
                }
            };
            // Check with the heap unlocked: the deadline check takes the
            // entry's target lock, and no thread ever holds both locks.
            drop(heap);
            if let Some(next_check) = check_deadline(&due) {
                self.register(WatchEntry { next_check, ..due });
            }
            heap = self.heap.lock().unwrap_or_else(|p| p.into_inner());
        }
    }
}

/// Re-read one popped entry's budgets. Fires (reason first, then terminate,
/// under the target lock) on an exceeded cap; returns the next check time
/// while the turn stays armed; `None` for a stale entry. A FIRED entry
/// keeps re-firing on a 10 ms cadence until the turn disarms — exactly the
/// old per-instance poll behavior: the export-extraction retry loop
/// re-enters guest getters after the first termination unwinds, and each
/// re-entered spin must be terminated again or extraction never finishes.
fn check_deadline(entry: &WatchEntry) -> Option<std::time::Instant> {
    let s = &entry.shared;
    // Cheap staleness pre-check without the lock — the common case at high
    // turn rates. Re-verified under the lock before anything fires.
    if s.generation.load(Ordering::Relaxed) != entry.generation {
        return None;
    }
    let guard = s.target.lock().unwrap_or_else(|p| p.into_inner());
    if s.generation.load(Ordering::Relaxed) != entry.generation {
        return None;
    }
    let t = guard.as_ref()?;
    let now = std::time::Instant::now();
    // One budget read per pop: it takes the executing thread's CPU clock,
    // under the lock that thread needs at every turn boundary.
    let cpu_ms = (t.cpu_cap_ms > 0).then(|| t.budget.elapsed_ms());
    if cpu_ms.is_some_and(|ms| ms >= t.cpu_cap_ms as u64) {
        s.reason.set(TerminationReason::Cpu); // first writer wins
        s.handle.terminate_execution();
        Some(now + Duration::from_millis(10))
    } else if t.wall_deadline.is_some_and(|d| now >= d) {
        s.reason.set(TerminationReason::Wall); // first writer wins
        s.handle.terminate_execution();
        Some(now + Duration::from_millis(10))
    } else {
        next_check_for(t, now, cpu_ms)
    }
}

/// The next moment worth re-reading `t`'s budgets: the earlier of the
/// estimated CPU expiry and the wall deadline. `None` when the target caps
/// nothing. Floored at 1 ms so a paused budget cannot spin the watchdog.
///
/// CPU advances at most at wall rate, so cap-minus-elapsed is a lower bound
/// on expiry, never late. It is optimistic for a starved turn, which re-arms
/// instead of firing; the estimate halves each round, so the extra checks are
/// logarithmic in the cap.
///
/// `cpu_elapsed_ms` is the caller's already-taken reading, `None` when
/// uncapped — the read takes a thread clock and is not worth repeating.
fn next_check_for(
    t: &GuardTarget,
    now: std::time::Instant,
    cpu_elapsed_ms: Option<u64>,
) -> Option<std::time::Instant> {
    let floor = now + Duration::from_millis(1);
    let cpu = cpu_elapsed_ms.map(|elapsed| {
        let remaining = (t.cpu_cap_ms as u64).saturating_sub(elapsed);
        (now + Duration::from_millis(remaining)).max(floor)
    });
    let wall = t.wall_deadline.map(|d| d.max(floor));
    match (cpu, wall) {
        (Some(c), Some(w)) => Some(c.min(w)),
        (c, w) => c.or(w),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU32;

    use super::*;
    use crate::v8::runloop::run_module;
    use crate::v8::testsupport::{call_spec, get_default, get_field, prepared, run_code, run_ok};
    use crate::testval::TestValue as WireValue;
    use crate::types;
    use crate::v8::{
        FailureOutput, Limits, Output, PrefixSpec, RunError, RunIo, create_instance_core,
        execute_with_prefix, init_platform, run_call_on_core,
    };

    // ── CPU and wall limits ───────────────────────────────────────────────

    #[test]
    fn infinite_loop_hits_cpu_timeout() {
        // CPU is a thread clock: under contention 200 ms of it can take seconds
        // of wall, so the wall limit is only a backstop against a dead CPU guard.
        let err = run_code(
            "while(true) {}",
            "<test>",
            Limits {
                cpu_time_ms: 200,
                wall_time_ms: 30_000,
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
        let prefix = prepared("export function spin() { while (true) {} }", &[], &[]);
        let err = execute_with_prefix(
            prefix,
            Limits {
                cpu_time_ms: 200,
                wall_time_ms: 1_000,
                ..Default::default()
            },
            &[],
            &[],
            RunIo::None,
            Arc::new(AtomicU32::new(0)),
            &call_spec("spin", &[]),
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

    #[test]
    fn wall_limit_kills_a_sync_spin_even_with_cpu_uncapped() {
        // The instance guard enforces min(remaining CPU, remaining wall)
        // MID-turn (E2 ruling): with `cpuTimeMs: 0` the wall deadline is the
        // only thing that can stop a synchronous spin, and it must not wait
        // for a turn boundary that never comes.
        let err = run_code(
            "for (;;) {}",
            "<test>",
            Limits {
                cpu_time_ms: 0,
                wall_time_ms: 200,
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

    // ── Result serialization stays under the run's budgets ──────────────────
    // V8's ValueSerializer runs guest getters (structured-clone semantics), so
    // serialization is guest execution: a getter that never returns must be
    // terminated by the still-armed guards. Each run is raced against a
    // channel timeout so a regression fails the test instead of hanging the
    // suite (the leaked spinner thread then dies with the test process).

    fn run_code_bounded(
        code: &'static str,
        limits: Limits,
        call: Option<types::CallSpec>,
    ) -> Result<Output, FailureOutput> {
        init_platform();
        let (tx, rx) = crossbeam_channel::bounded(1);
        std::thread::spawn(move || {
            let result = run_module(
                code,
                "<test>",
                limits,
                &[],
                &[],
                RunIo::None,
                0,
                Arc::new(AtomicU32::new(0)),
                call.as_ref(),
                None,
                Arc::new(crate::link::NoopObserver),
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

    #[test]
    fn fired_guard_taints_the_core() {
        init_platform();
        let mut core = create_instance_core(
            PrefixSpec {
                code: "export function spin() { for (;;) {} }",
                filename: "<prefix>",
                globals: &[],
            },
            &[],
            0,
            crate::v8::OomRunLabel::NONE,
        )
        .expect("spin prefix warms up");
        let call = call_spec("spin", &[]);
        let outcome = run_call_on_core(
            &mut core,
            &[],
            Limits {
                cpu_time_ms: 50,
                wall_time_ms: 5_000,
                ..Default::default()
            },
            &[],
            &[],
            RunIo::None,
            Arc::new(AtomicU32::new(0)),
            &call,
        );
        assert!(outcome.tainted, "a fired guard must taint the instance");
        let failure = outcome.result.unwrap_err();
        assert!(matches!(
            failure.error,
            RunError::CpuTimeout | RunError::WallTimeout
        ));
    }
}
