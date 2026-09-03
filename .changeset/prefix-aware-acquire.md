---
"@iso4/sandbox": minor
---

feat: prefix runs join busy instances; instances scale with measured CPU demand (#77)

A prefix run with no idle instance now joins a busy one (the engine
interleaves runs) instead of always cold-starting, and another isolate is
opened only when the prefix's measured CPU demand justifies it — so
waiting-heavy traffic stops piling up isolates while compute-heavy traffic
still scales out for response time. `SandboxStats.activeRuns` now counts
runs (several can share one instance); `warmInstances` counts instances as
before.
