---
"@iso4/sandbox": minor
---

feat: capacity manager — memory budget and `sandbox.stats()` (#65)

New `memoryBudgetMb` decides how many isolates stay alive, container-aware by
default, while `maxIsolates` still caps concurrent runs. `sandbox.stats()`
returns a capacity snapshot over a control connection outside the run pool.
