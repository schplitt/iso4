---
"@iso4/sandbox": minor
---

feat(native): name the run and prefix on stderr before a fatal V8 OOM abort

The runtime child now prints `[iso4-v8] fatal: V8 out of memory … in run <id> of prefix <id> …` before V8 aborts the process. V8's own `Fatal JavaScript out of memory` block no longer appears; the line above carries its message.
