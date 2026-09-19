---
'@iso4/sandbox': patch
---

fix: `cpuTimeMs` measures CPU rather than elapsed time

A run was charged for time its thread sat waiting for a core, so the same guest code read 2.1 ms with 6 runs in flight and 86.1 ms with 256 — and the `cpuTimeMs` cap fired on that inflated figure, failing runs with `ERR_CPU_TIMEOUT` for CPU they never used.

Expect reported `cpuTimeMs` to drop sharply on a busy host and to stop moving with how many runs share it. A starved run now gets its full allowance and takes longer in elapsed terms, still bounded by `wallTimeMs`.
