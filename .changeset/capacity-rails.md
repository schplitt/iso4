---
"@iso4/sandbox": minor
---

feat: memory capacity rails — global-container metering, a hard admission line, and bounded queueing (#77)

Memory watermarks now measure the whole container (cgroup working set,
Node host included) instead of the runtime child alone, the budget default
becomes 80% of the container limit minus a 256 MB host reserve, and a new
isolate is never created when measured usage plus the run's `memoryMb`
would cross 90% of that base — such runs fail with the new `ERR_CAPACITY`
code instead of queueing. `maxConcurrentRuns`' automatic default is now
memory-bounded, and the new `maxQueuedRuns` (default 100 × slots) sheds
callers past the queue bound with the new `ERR_QUEUE_FULL` code.
