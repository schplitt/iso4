---
"@iso4/sandbox": patch
---

fix(sandbox)!: `sandbox.run()`, `prefix.run()` and `prefix.call()` now throw setup errors (`SetupErrorCode`) like `prepare()` instead of returning `{ ok: false }`, checked on the client before anything is sent.
Undeclared or frozen data globals on prefix runs now throw instead of being ignored, and a limit that is not an integer from 0 to 2³²−1 throws a `TypeError`.
