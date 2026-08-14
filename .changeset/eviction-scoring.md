---
"@iso4/sandbox": minor
---

feat: RSS watermark + scored eviction — `heapUsed × idleTime` (#66)

The memory budget is now enforced against reality, with celld's pressure
model taken whole: **one mark**. The runtime watches its **own process
RSS** — the number the container OOM killer acts on — instead of counting
isolates at a worst-case `budget ÷ memoryMb`. Summed heap numbers
undercount what the OS charges (external ArrayBuffers, V8 overhead,
allocator fragmentation); RSS is ground truth. Sampled per registry event
(~0.4 µs), no polling timers.

- **RSS at/above `memoryBudgetMb`**: idle warm instances are evicted by
  `heapUsed × idleTime` score, highest first (a young 90 MB hoarder goes
  before an old 3 MB idler), AND new warm admissions stop — prefix runs
  without an idle instance execute on cold one-off isolates. Reusing an
  already-warm instance stays allowed; it adds no memory. Calls keep
  succeeding — correctness never depends on warmth, the only observable
  cost is cold-start latency. No new error code.
- **The latch releases at 80 % of the budget** — hysteresis, so eviction
  doesn't flap. A pass that leaves RSS flat stops walking (freed heap
  returns to the OS lazily) instead of evicting the world.
- **No grace period**: the idleTime factor already sends a just-used
  instance to the back of every pass (recorded on #66).

**The instance-count cap is gone.** #65's `budget ÷ memoryMb` live cap
allowed exactly two 128 MB-capped isolates on a 256 MB budget even if they
really used 3 MB each; now a healthy budget keeps *every* prefix warm and
memory, not arithmetic, decides. (celld defaults its resident ceiling to
unlimited after a default count cap caused eviction churn.) The budget is
also independent of `memoryMb` now — RSS is measured, not derived — so
`memoryBudgetMb` works with uncapped isolates, and `memoryBudgetMb: 0`
disables the mark entirely.

**`sandbox.stats()`** gains `budgetBytes` (the mark), `rssBytes` (the
signal), and `underPressure` (the latch); `maxLiveIsolates` is gone with
the count cap. Wire: `StatsPayload` swaps the u32 cap for the two u64
marks + a latch byte; protocol stays v2 (pre-release lockstep releases).

The policy lives in pure decision functions (`policy.rs`) — facts in,
verdict out, unit-tested without clocks or allocations, and
CodSpeed-tracked (`watermark_action` ~1 ns per registry event; victim
scoring runs only when a pass actually evicts).
