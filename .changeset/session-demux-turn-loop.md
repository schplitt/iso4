---
"@iso4/sandbox": minor
"@iso4/v8-darwin-arm64": minor
"@iso4/v8-darwin-x64": minor
"@iso4/v8-linux-arm64-gnu": minor
"@iso4/v8-linux-x64-gnu": minor
---

feat: session demux and per-instance turn loop (#125)

Runs no longer hold a thread while suspended on host calls, and a waiting
run's abort or timeout now fails that run alone instead of evicting its
warm instance. New error code
`ERR_INSTANCE_RESET` (with `resetCause` and `culpritRunId` on the error)
reports runs that were in flight on a shared instance when a co-resident run
had to be terminated mid-execution.
