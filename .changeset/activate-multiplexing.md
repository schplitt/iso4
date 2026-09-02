---
"@iso4/sandbox": minor
---

feat: runs share connections, and a run's slot frees at its Result (#127)

Concurrent runs are multiplexed onto shared connections (several per
socket, routed by run id) instead of opening one connection each, and a run
with pending `waitUntil` work no longer holds admission capacity during its
background grace phase. `maxConcurrentRuns` semantics are unchanged;
`SandboxStats.openConnections` now tracks roughly peak concurrency divided
by the per-connection share instead of peak concurrency.
