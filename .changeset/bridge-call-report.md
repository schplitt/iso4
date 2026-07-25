---
'@iso4/sandbox': minor
'@iso4/v8-darwin-arm64': minor
'@iso4/v8-darwin-x64': minor
'@iso4/v8-linux-x64-gnu': minor
'@iso4/v8-linux-arm64-gnu': minor
---

Every `RunResult` now reports what the run did, measured inside the Rust runtime: `cpuTimeMs` (active V8 execution time, bridge waits excluded — `durationMs` remains wall-clock time) and `bridgeCalls`, one metadata record per bridge call attempt in attempt order — resolved name (`fetch`, `tools:search.query`), start offset and round-trip duration on the run's clock, argument/response byte sizes, and outcome. Attempts blocked by `maxBridgeCalls`/`maxBridgeCallBytes` are included with `blocked: true`, so runs that hit a limit show exactly which attempt crossed it. Payloads are never captured, keeping the report cheap enough to stay always-on. Aborted runs report zeros and an empty list for now (the abort teardown precedes the runtime's result frame; see #36).
