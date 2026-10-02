---
"@iso4/sandbox": patch
---

fix(sandbox): refuse host-module shapes nested deeper than 64 levels

An `imports` host-module shape may nest at most 64 levels. A deeper shape now fails `prepare()`/`run()` on the host with the offending path instead of crashing the shared runtime process.
