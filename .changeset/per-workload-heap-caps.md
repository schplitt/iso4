---
"@iso4/sandbox": minor
---

feat: per-prefix and per-one-off heap caps (#77)

`prepare({ memoryMb })` caps every isolate serving that prefix, and one-off
`run()` accepts `limits.memoryMb` again for its fresh isolate — the
sandbox-level `memoryMb` becomes the default for both. Prefix
`execute()`/`call()` still reject a per-run value: warm isolates are shared
and their cap is fixed at creation.
