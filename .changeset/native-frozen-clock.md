---
"@iso4/sandbox": patch
---

perf(native): freeze V8's own clock instead of shimming Date, Intl and Temporal

`Date`, `Intl.DateTimeFormat` and `Temporal.Now` are now the unmodified V8 builtins (`[native code]`); the frozen-clock behavior is unchanged.
