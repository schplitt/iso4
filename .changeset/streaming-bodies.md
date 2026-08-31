---
"@iso4/sandbox": patch
---

feat: stream large Request/Response bodies into the sandbox

A host body that outgrows a 64 KiB probe now crosses as a stream pumped under flow control instead of being buffered whole: lower memory, and the sandbox starts reading on the first chunk via `.body` or the body helpers. Small bodies keep the buffered path unchanged; returning a streamed body to the host still requires reading it first.
