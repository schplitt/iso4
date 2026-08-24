---
"@iso4/sandbox": patch
---

fix: Request/Response clone() no longer shares its body buffer

A cloned Request or Response now gets its own copy of a buffer body, so
mutating one side's bytes no longer reaches through to the other.
