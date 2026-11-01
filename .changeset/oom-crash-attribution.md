---
"@iso4/sandbox": patch
---

feat(native): name the run and prefix on stderr before a fatal V8 OOM abort

The runtime child now prints `[iso4-v8] fatal: V8 out of memory … in run <id> of prefix <id> …` before V8 aborts the process. This line replaces V8's own `Fatal JavaScript out of memory` block; the abort banner now reads `API fatal error handler returned after process out of memory`.
