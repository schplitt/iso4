---
"@iso4/sandbox": minor
---

feat: warm isolate registry — prefix runs reuse resident isolates (#64)

Warmth is a cache and never a guarantee: module-scope state may survive between
runs on an instance and may be evicted at any time. Breaking — `limits.memoryMb`
moves to `createSandbox({ memoryMb })` with the default raised to 128 MB; new
`ERR_WARMUP_LIMIT` and a new `heapUsedBytes` result field.
