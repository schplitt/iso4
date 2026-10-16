---
"@iso4/sandbox": patch
---

perf(native): boot every sandbox from a build-time V8 snapshot (#83)

The runtime's built-in JavaScript is no longer compiled and run for each new sandbox, so one-off runs and cold starts are faster. Nothing changes for guest code.
