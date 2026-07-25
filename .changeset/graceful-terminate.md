---
'@iso4/sandbox': minor
'@iso4/v8-darwin-arm64': minor
'@iso4/v8-darwin-x64': minor
'@iso4/v8-linux-x64-gnu': minor
'@iso4/v8-linux-arm64-gnu': minor
---

Aborting a run now terminates it gracefully whenever possible, so the aborted `RunResult` carries real telemetry instead of synthesized zeros. Previously an abort destroyed the socket and TypeScript fabricated the result (`durationMs: 0`, `cpuTimeMs: 0`, `bridgeCalls: []`) — which meant every durable-isolates suspension (implemented as an abort) lost its per-call bridge records. Now TypeScript sends a `Terminate` frame carrying the run ID; the Rust runtime, parked in its bridge-wait poll loop, stops the run and replies with a real `ERR_ABORTED` result reporting `durationMs`, `cpuTimeMs`, and the bridge-call records collected up to the abort. The connection stays healthy and is reused rather than reconnected. This covers the case that matters most — suspensions always happen while the sandbox is awaiting a bridge call, exactly when the runtime can receive the frame. A run stuck in a tight synchronous loop can't be reached this way, so after a short grace period TypeScript falls back to today's socket teardown and synthesized (zeroed) result; the busy isolate is still reclaimed by its CPU guard.
