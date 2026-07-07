---
"@iso4/sandbox": minor
---

feat: honor AbortSignal during an in-flight run

`sandbox.run({ signal })` and `prefix.run({ signal })` now observe an `AbortSignal` that fires **while a run is executing** — previously the signal was only checked once at run entry, so a mid-run `controller.abort()` was a no-op until the run finished naturally or hit `wallTimeMs`.

Aborting mid-run now:

- Resolves the run with `{ ok: false, error: { code: 'ERR_ABORTED', name: 'AbortError' } }` promptly, without waiting for `wallTimeMs`.
- Works while a host bridge call is in flight: the connection is torn down so the Rust isolate is reclaimed, and any late `BridgeResponse` the handler produces is discarded (the sandbox never observes a return value for the call that was in flight when the abort landed). This makes `controller.abort()` from inside a bridge handler a spoof-proof way to stop a run — sandbox code cannot catch or swallow it.
- Leaves the pool healthy: the torn-down slot is replaced with a fresh connection, so other runs and subsequent runs are unaffected.

Note: aborting a purely CPU-bound run resolves the `run()` promise immediately, but the abandoned isolate is only reclaimed when its CPU guard fires (bounded by `cpuTimeMs`, not `wallTimeMs`).
