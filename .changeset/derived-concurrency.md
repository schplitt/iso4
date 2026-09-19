---
'@iso4/sandbox': patch
---

feat: the sandbox derives its own concurrency from the work it is serving

Left unset, `maxConcurrentRuns` no longer means one run per core: the runtime sizes concurrency from the shape of the runs completing and follows it as the workload changes. Setting it pins the number instead — leave it unset unless you have measured this workload on this hardware.

Two things to check before upgrading: `maxQueuedRuns` now defaults to a flat `10_000` rather than `100 × maxConcurrentRuns`, and `ERR_CAPACITY` is renamed `ERR_CAPACITY_MEMORY`. `SandboxStats` gains `slotLimit`.
