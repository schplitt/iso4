---
"@iso4/sandbox": patch
---

feat: `waitUntil()` lets a run finish background work after its result

Sandbox code can register background work with the new `waitUntil(promise)` global (also importable from `iso4:runtime`); the caller gets the result immediately and the work keeps running up to a configurable grace budget (`limits.graceMs`, default 30 s). The outcome arrives as `result.waitUntil`, a never-rejecting promise with status and telemetry.
