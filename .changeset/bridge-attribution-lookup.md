---
"@iso4/sandbox": patch
"@iso4/v8-darwin-arm64": patch
"@iso4/v8-darwin-x64": patch
"@iso4/v8-linux-arm64-gnu": patch
"@iso4/v8-linux-x64-gnu": patch
---

perf: cheaper per-call attribution for bridge calls and console output (#127)

Native callbacks resolve the owning run in one table lookup and take a
reference-counted handle to the stub binding instead of cloning it per
invocation. Attribution semantics are unchanged.
