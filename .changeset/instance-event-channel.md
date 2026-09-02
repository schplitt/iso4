---
"@iso4/sandbox": patch
"@iso4/v8-darwin-arm64": patch
"@iso4/v8-darwin-x64": patch
"@iso4/v8-linux-arm64-gnu": patch
"@iso4/v8-linux-x64-gnu": patch
---

perf: instance turn loops route run events through one channel and a deadline heap (#127)

Frame routing on a busy instance no longer scales with the number of
in-flight runs, and boundary deadlines fire in arrival order: co-resident
frame traffic can neither starve a run's wall timeout nor turn a run whose
answer arrived in time into one.
