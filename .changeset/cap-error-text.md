---
"@iso4/sandbox": patch
---

fix(native): cap error text copied out of V8 at 64 Ki UTF-16 units (#250)

A huge thrown message, stack or console line no longer crashes the runtime; the copied text ends with `… [truncated by iso4: N UTF-16 units total]`.
