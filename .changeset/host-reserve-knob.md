---
'@iso4/sandbox': minor
---

feat: `hostReserveMb` — the host's share of the container limit is now a knob

Both capacity lines are drawn from the container limit minus a reserve for the host process: the default memory budget is 80 % of what it leaves, the runtime's admission line 90 %. That reserve was a constant mirrored in two places (128 MB); it is now the `hostReserveMb` option, passed to the runtime instead of mirrored, so the two lines can no longer be derived from different bases. The default is unchanged at 128 MB — it is headroom for the host's GROWTH, not its current usage, which the global container meter already counts, so a host that caches heavily wants more and a thin one can hand the sandbox the whole limit with `0`. The refusal message keeps quoting the reserve actually in force.

`createSandbox` also warns on stderr when an explicit `maxConcurrentRuns` promises more concurrent heap than the container can hold — slots × the run's terminating ceiling over the memory budget, or over the admission line, where the consequence is routine `ERR_CAPACITY`. It never throws: the ceiling is a worst case, and `memoryMb: 0` or `memoryBudgetMb: 0` opts out.
