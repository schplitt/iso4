---
"@iso4/sandbox": patch
---

refactor(native): a prefix's heap cap is sent once at `prepare()` and stored by the runtime

`Precompile` carries the heap cap and `PrefixRun` no longer does, so every run on a prefix uses the cap set at prepare. The limits travel as two wire blocks, heap and run limits. No API change.
