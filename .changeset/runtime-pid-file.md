---
"@iso4/sandbox": minor
---

feat(native): the runtime writes a `pid` file next to its socket

It fails to start if it cannot write that file or read its own start time, which on Linux needs a readable `/proc`.
