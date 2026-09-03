//! Eviction and memory-pressure policy: pure decision functions.
//!
//! Every function here takes its facts as parameters and returns a verdict —
//! no clock reads, no RSS reads, no registry access. `warm.rs` gathers the
//! facts under its lock, calls in, and performs what comes back. That split
//! keeps the acceptance tests plain unit tests (a hand-written fact slice in,
//! an assertion on the verdict out) and makes the rules replaceable, which
//! a future prefix-aware acquire policy will rely on. Pattern reference: denoland/celld
//! `crates/logic` (`pressure.rs` for the watermark mechanics).
//!
//! The pressure model is celld's, whole (decided 2026-08-14): ONE mark.
//! The warm budget (`memoryBudgetMb` → `--warm-budget-bytes`, celld's
//! `CELLD_MAX_RSS_MB` shape, 0 disables) is the ceiling: usage at/above it
//! latches shedding — evict idle instances by score AND stop pooling new
//! ones (a run without an idle instance goes cold one-off; reuse stays
//! allowed, it adds no memory) — and the latch releases at 4/5 of the
//! ceiling, the hysteresis gap that stops evict/admit flapping. There is
//! deliberately NO instance-count cap (celld defaults its resident
//! ceiling to `usize::MAX`; a default count cap caused them eviction
//! churn) and no grace period after last use — the `heapUsed × idleTime`
//! score already sends a just-used instance to the back of every pass
//! (celld sheds in plain LRU order for the same reason).
//!
//! Above the budget sits ONE more line (#77): the hard admission line at
//! 90% of (container limit − Node reserve). [`admit_isolate`] refuses to
//! CREATE an isolate that could tip the container into an OOM kill; it
//! never gates reuse. Both rules read measured GLOBAL container memory —
//! see `container.rs`.

use std::time::Instant;

/// What the victim picker knows about one idle instance. Busy instances are
/// never candidates and must not appear in the slice.
#[derive(Clone, Copy, Debug)]
pub struct VictimFact {
    /// `used_heap_size` reported when the instance last finished a call.
    pub heap_used_bytes: u64,
    /// When the instance last finished a call.
    pub last_used: Instant,
}

/// Pick the instance to evict: highest `heapUsed × idleTime`, ties to the
/// longest-idle. Returns an index into `idle`, or `None` when it is empty.
///
/// The product prefers a young hoarder over an old small idler once the
/// heap difference outweighs the age difference, and degenerates to plain
/// LRU when heap sizes are equal (including the all-zero accounting case).
/// Among byte-identical facts the caller's slice order decides — callers
/// that need stable output across runs must present a stable order.
pub fn pick_victim(idle: &[VictimFact], now: Instant) -> Option<usize> {
    idle.iter()
        .enumerate()
        .max_by(|(_, a), (_, b)| {
            score(a, now)
                .cmp(&score(b, now))
                // Equal scores: the earlier `last_used` (longer idle) wins.
                .then_with(|| b.last_used.cmp(&a.last_used))
        })
        .map(|(i, _)| i)
}

/// Nanosecond resolution: back-to-back releases are far less than a
/// microsecond apart, and a coarser clock would zero their idle times and
/// push the choice into the tie-break. Saturating: a pathological heap ×
/// a pathological age may exceed even u128, and "maximal score" is the
/// right answer there, not a panic.
fn score(fact: &VictimFact, now: Instant) -> u128 {
    let idle_nanos = now.saturating_duration_since(fact.last_used).as_nanos();
    u128::from(fact.heap_used_bytes).saturating_mul(idle_nanos)
}

/// What the last completed shed pass measured — the futility check compares
/// the next usage sample against this. Passes are synchronous (victims are
/// dropped under the registry lock), so a recorded pass has always "landed";
/// only the OS's lazy page reclaim lags behind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PassOutcome {
    /// The usage sample the pass acted on.
    pub usage_at_pass: u64,
}

/// Everything the watermark decision reads. Gathered under the registry
/// lock so the counts are consistent with each other.
#[derive(Clone, Copy, Debug)]
pub struct PressureFacts {
    /// Measured GLOBAL container memory (cgroup working set, Node host
    /// included — the number the OOM killer charges; child RSS where no
    /// cgroup exists). Not summed heap numbers — those undercount:
    /// external ArrayBuffers, V8 overhead, allocator fragmentation.
    pub usage_bytes: u64,
    /// The warm budget: the one mark. Shedding latches at/above it and
    /// releases at 4/5 of it. `0` disables watermarks entirely.
    pub budget_bytes: u64,
    /// The shedding latch as of the previous verdict.
    pub was_shedding: bool,
    /// The last completed shed pass, `None` once the latch released.
    pub last_pass: Option<PassOutcome>,
    /// Idle instances currently evictable.
    pub idle_count: usize,
}

/// The watermark verdict. The caller stores `shedding` back as the next
/// call's `was_shedding`, drops `evict` victims (chosen by [`pick_victim`]),
/// records a [`PassOutcome`] if it dropped any, and clears `last_pass`
/// whenever `shedding` is false.
///
/// While `shedding` is true the caller also refuses to pool NEW instances
/// (celld: a node over its ceiling "may keep serving on the isolates it
/// already has, but must not build another") — reuse of existing warmth
/// stays allowed, it adds no memory. Shedding and the admission stop are
/// deliberately ONE state, not two marks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PressureVerdict {
    /// The latch: RSS reached the budget, or a prior crossing has not yet
    /// fallen back to 4/5 of it.
    pub shedding: bool,
    /// How many idle instances this pass should evict (0 = none).
    pub evict: usize,
}

/// Fold one RSS sample into the watermark state.
///
/// Mechanics from celld's `pressure.rs` / `load_sampled`:
/// - one mark with hysteresis — act at/above the budget, keep shedding
///   until at/below 4/5 of it, so eviction doesn't flap;
/// - per-pass shed target derived from what is actually evictable
///   (a tenth of the idle population, at least one);
/// - futility check — when the last pass left RSS flat (within 5 %),
///   stop walking instead of evicting the world: freed heap returns to the
///   OS lazily, and a budget below the process's floor must not empty the
///   pool for nothing. The latch HOLDS while the walk stops (admission
///   stays closed); a sample that moves either way re-arms the walk.
pub fn watermark_action(facts: &PressureFacts) -> PressureVerdict {
    if facts.budget_bytes == 0 {
        return PressureVerdict {
            shedding: false,
            evict: 0,
        };
    }
    let release = facts.budget_bytes / 5 * 4;
    let shedding = facts.usage_bytes >= facts.budget_bytes
        || (facts.was_shedding && facts.usage_bytes > release);
    if !shedding || facts.idle_count == 0 {
        return PressureVerdict { shedding, evict: 0 };
    }
    if let Some(last) = facts.last_pass {
        let flat = facts.usage_bytes.abs_diff(last.usage_at_pass) <= last.usage_at_pass / 20;
        if flat {
            return PressureVerdict { shedding, evict: 0 };
        }
    }
    PressureVerdict {
        shedding,
        evict: shed_target(facts.idle_count),
    }
}

/// A tenth of the idle population per pass, at least one. A proportion of
/// what was just measured, because an eviction's effect on the sample is
/// not visible until a later one — one-at-a-time feedback would stall.
fn shed_target(idle_count: usize) -> usize {
    (idle_count / 10).max(1)
}

/// Everything the isolate-admission decision reads.
#[derive(Clone, Copy, Debug)]
pub struct AdmitFacts {
    /// Measured global container usage (same meter as [`PressureFacts`]).
    pub usage_bytes: u64,
    /// The run's own heap cap (`memoryMb`) in bytes. `0` = uncapped.
    pub run_cap_bytes: u64,
    /// The hard admission line — `container::admission_line_bytes()`.
    /// `0` = no container limit readable, the line is disabled.
    pub hard_line_bytes: u64,
    /// The warm budget (the 80% mark) — the refusal line for UNCAPPED
    /// runs, whose worst case no arithmetic can bound. `0` = disabled.
    pub budget_bytes: u64,
}

/// May a NEW isolate be created for this run? (#77 ruling: never create an
/// isolate whose own cap could tip the container over the hard line — the
/// newest admission always leaves one worst-case isolate of headroom.)
///
/// - Capped run: admit iff `usage + cap` stays at or below the hard line.
/// - Uncapped run (`memoryMb: 0`): the sum is meaningless, so the budget
///   mark is the refusal line instead — the riskiest heap is not admitted
///   once memory is already tight (ruled 2026-09-03). Budget disabled ⇒
///   the caller opted out of protection ⇒ admit.
///
/// Reuse/join is deliberately NOT gated here: an existing isolate adds no
/// memory.
pub fn admit_isolate(facts: &AdmitFacts) -> bool {
    if facts.run_cap_bytes == 0 {
        return facts.budget_bytes == 0 || facts.usage_bytes < facts.budget_bytes;
    }
    facts.hard_line_bytes == 0
        || facts.usage_bytes.saturating_add(facts.run_cap_bytes) <= facts.hard_line_bytes
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const MB: u64 = 1024 * 1024;

    fn fact(heap_mb: u64, idle: Duration, now: Instant) -> VictimFact {
        VictimFact {
            heap_used_bytes: heap_mb * MB,
            last_used: now - idle,
        }
    }

    #[test]
    fn scoring_prefers_a_young_hoarder_over_a_modest_idler() {
        let now = Instant::now();
        // 90 MB × 10 s ≈ 9.4e14 outweighs 3 MB × 60 s ≈ 1.9e14.
        let idle = [
            fact(3, Duration::from_secs(60), now),
            fact(90, Duration::from_secs(10), now),
        ];
        assert_eq!(pick_victim(&idle, now), Some(1));
    }

    #[test]
    fn scoring_prefers_a_long_idler_over_a_just_used_hoarder() {
        let now = Instant::now();
        // 1 MB × 1 h ≈ 3.8e15 outweighs 50 MB × 1 ms ≈ 5.2e10: enough age
        // beats any heap advantage — the product works in both directions.
        let idle = [
            fact(50, Duration::from_millis(1), now),
            fact(1, Duration::from_secs(3600), now),
        ];
        assert_eq!(pick_victim(&idle, now), Some(1));
    }

    #[test]
    fn equal_scores_evict_the_longest_idle() {
        let now = Instant::now();
        // Same heap, different ages → same-score tie is impossible here, so
        // force it with zero heap: every score is 0 and age must decide.
        let idle = [
            fact(0, Duration::from_secs(1), now),
            fact(0, Duration::from_secs(30), now),
            fact(0, Duration::from_secs(5), now),
        ];
        assert_eq!(pick_victim(&idle, now), Some(1));
    }

    #[test]
    fn zero_heap_degenerates_to_lru() {
        let now = Instant::now();
        // The accounting-test case: no call ever ran, all heaps are 0. The
        // pick must match the old evict_lru choice (least recently used).
        let idle = [
            fact(0, Duration::from_secs(2), now),
            fact(0, Duration::from_secs(7), now),
        ];
        assert_eq!(pick_victim(&idle, now), Some(1));
    }

    #[test]
    fn no_idle_instances_no_victim() {
        assert_eq!(pick_victim(&[], Instant::now()), None);
    }

    fn facts(
        rss_mb: u64,
        budget_mb: u64,
        was_shedding: bool,
        last_pass: Option<PassOutcome>,
        idle_count: usize,
    ) -> PressureFacts {
        PressureFacts {
            usage_bytes: rss_mb * MB,
            budget_bytes: budget_mb * MB,
            was_shedding,
            last_pass,
            idle_count,
        }
    }

    // Budget 100 MB → the mark is 100 MB, the release line 80 MB.

    #[test]
    fn below_the_mark_nothing_happens() {
        let v = watermark_action(&facts(99, 100, false, None, 20));
        assert_eq!(
            v,
            PressureVerdict {
                shedding: false,
                evict: 0
            }
        );
    }

    #[test]
    fn reaching_the_mark_starts_a_shed_pass() {
        let v = watermark_action(&facts(100, 100, false, None, 20));
        assert_eq!(
            v,
            PressureVerdict {
                shedding: true,
                evict: 2 // 20 idle / 10
            }
        );
    }

    #[test]
    fn between_release_and_mark_the_latch_holds() {
        // 90 MB is under the mark (100) but over the release line (80):
        // shedding continues only because it already started.
        let v = watermark_action(&facts(90, 100, true, None, 20));
        assert!(v.shedding);
        assert_eq!(v.evict, 2);
        let fresh = watermark_action(&facts(90, 100, false, None, 20));
        assert!(!fresh.shedding, "no crossing, no shedding — no flapping");
    }

    #[test]
    fn at_or_below_the_release_line_the_latch_opens() {
        let v = watermark_action(&facts(80, 100, true, None, 20));
        assert_eq!(
            v,
            PressureVerdict {
                shedding: false,
                evict: 0
            }
        );
    }

    #[test]
    fn shed_target_is_a_tenth_of_idle_at_least_one() {
        assert_eq!(watermark_action(&facts(100, 100, false, None, 35)).evict, 3);
        assert_eq!(watermark_action(&facts(100, 100, false, None, 5)).evict, 1);
        assert_eq!(watermark_action(&facts(100, 100, false, None, 1)).evict, 1);
    }

    #[test]
    fn nothing_idle_nothing_to_evict() {
        let v = watermark_action(&facts(100, 100, false, None, 0));
        assert!(v.shedding, "the latch holds even with nothing to shed");
        assert_eq!(v.evict, 0);
    }

    #[test]
    fn a_flat_sample_after_a_pass_stops_the_walk_but_not_the_latch() {
        // Last pass acted on 100 MB; the new sample is within 5 % of it —
        // the OS has not returned the freed pages, so keep the latch (RSS
        // genuinely is over the mark) but stop evicting.
        let last = Some(PassOutcome {
            usage_at_pass: 100 * MB,
        });
        let v = watermark_action(&facts(102, 100, true, last, 18));
        assert!(v.shedding);
        assert_eq!(v.evict, 0, "futile: RSS did not move");
    }

    #[test]
    fn a_moved_sample_rearms_the_walk() {
        let last = Some(PassOutcome {
            usage_at_pass: 100 * MB,
        });
        // Moved up well past the 5 % band: evict again.
        let up = watermark_action(&facts(110, 100, true, last, 18));
        assert_eq!(up.evict, 1);
        // Moved down but still latched (over the release line): also
        // re-arms — celld: "a sample that moves either way re-arms the
        // walk down".
        let down = watermark_action(&facts(90, 100, true, last, 18));
        assert_eq!(down.evict, 1);
    }

    #[test]
    fn zero_budget_disables_watermarks() {
        let v = watermark_action(&facts(10_000, 0, true, None, 50));
        assert_eq!(
            v,
            PressureVerdict {
                shedding: false,
                evict: 0
            }
        );
    }

    fn admit(usage_mb: u64, cap_mb: u64, line_mb: u64, budget_mb: u64) -> bool {
        admit_isolate(&AdmitFacts {
            usage_bytes: usage_mb * MB,
            run_cap_bytes: cap_mb * MB,
            hard_line_bytes: line_mb * MB,
            budget_bytes: budget_mb * MB,
        })
    }

    // Line 900 MB, budget 800 MB — the shipped 90/80 shape.

    #[test]
    fn a_capped_run_is_admitted_while_it_fits_under_the_line() {
        assert!(admit(700, 128, 900, 800));
        // Landing exactly ON the line still leaves the promised headroom.
        assert!(admit(772, 128, 900, 800));
    }

    #[test]
    fn a_capped_run_that_could_tip_the_container_is_refused() {
        assert!(!admit(773, 128, 900, 800));
        // Small runs stay admissible when big ones are not (#77 comment).
        assert!(admit(773, 64, 900, 800));
        assert!(!admit(2000, 1, 900, 800), "already over the line");
    }

    #[test]
    fn no_readable_limit_disables_the_line_for_capped_runs() {
        assert!(admit(u64::MAX / MB / 2, 128, 0, 800));
    }

    #[test]
    fn an_uncapped_run_is_refused_once_usage_reaches_the_budget() {
        assert!(admit(799, 0, 900, 800), "below the budget: admissible");
        assert!(!admit(800, 0, 900, 800), "at the budget: refused");
        assert!(!admit(850, 0, 900, 800), "above: refused even under the line");
    }

    #[test]
    fn an_uncapped_run_with_watermarks_disabled_is_admitted() {
        // budget 0 = the embedder opted out of memory protection entirely.
        assert!(admit(10_000, 0, 900, 0));
    }
}
