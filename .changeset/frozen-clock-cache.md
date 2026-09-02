---
"@iso4/sandbox": patch
"@iso4/v8-darwin-arm64": patch
"@iso4/v8-darwin-x64": patch
"@iso4/v8-linux-arm64-gnu": patch
"@iso4/v8-linux-x64-gnu": patch
---

perf: the frozen-clock advance no longer allocates V8 state per turn (#127)

Clock handles are cached per isolate at install time; a turn within the
same wall millisecond now skips V8 entirely. Clock semantics are unchanged.
