---
"@iso4/sandbox": patch
---

fix: run ids are numbered per sandbox instead of per connection

A run id now names one run in the runtime, so `run=N` on the crash line and `culpritRunId` on errors point at a single run.
