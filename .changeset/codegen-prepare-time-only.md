---
"@iso4/sandbox": patch
---

feat: disable eval and new Function in run code

Code generation from strings is now a prepare()-time capability: setup code can still compile functions from strings, but per-run code calling `eval` or `new Function` gets a catchable `EvalError` and the run continues.
