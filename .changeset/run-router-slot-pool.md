---
"@iso4/sandbox": minor
---

feat: run-slot admission, lazy connections, and a per-run frame router (#126)

Breaking: `maxIsolates` is replaced by `maxConcurrentRuns` (same default) — it caps runs executing at once, the rest queue FIFO, and `AbortSignal.timeout()` bounds the wait. Connections to the runtime now open on demand instead of all at `createSandbox()`, and `stats()` gains `openConnections`.
