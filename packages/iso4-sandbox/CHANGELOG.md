# @iso4/sandbox

## 0.2.0

### Minor Changes

- 0fca967: chore: add `status` discriminant and abort `reason` to `RunResult`

  `RunResult` now carries an explicit `status: 'completed' | 'failed' | 'aborted'`, promoting a deliberate abort to a first-class outcome instead of leaving it indistinguishable from a genuine failure. Aborted runs additionally expose `reason` — the value passed to `AbortController.abort(reason)`.

  This is **additive and backward compatible**:

  - `ok` stays as a convenience alias for `status === 'completed'`, so `if (result.ok)` is unchanged.
  - Aborted results keep `error` with `code: 'ERR_ABORTED'`, so existing `!result.ok && result.error.code === 'ERR_ABORTED'` checks keep working.
  - New code can switch on `result.status` and, for aborts, read `result.reason`.

  ```ts
  const result = await sandbox.run({ code, signal });
  switch (result.status) {
    case "completed":
      use(result.exports);
      break;
    case "failed":
      handle(result.error);
      break;
    case "aborted":
      suspend(result.reason);
      break; // reason = whatever abort(reason) received
  }
  ```

## 0.1.0

### Minor Changes

- 2cdad7a: feat: honor AbortSignal during an in-flight run

  `sandbox.run({ signal })` and `prefix.run({ signal })` now observe an `AbortSignal` that fires **while a run is executing** — previously the signal was only checked once at run entry, so a mid-run `controller.abort()` was a no-op until the run finished naturally or hit `wallTimeMs`.

  Aborting mid-run now:

  - Resolves the run with `{ ok: false, error: { code: 'ERR_ABORTED', name: 'AbortError' } }` promptly, without waiting for `wallTimeMs`.
  - Works while a host bridge call is in flight: the connection is torn down so the Rust isolate is reclaimed, and any late `BridgeResponse` the handler produces is discarded (the sandbox never observes a return value for the call that was in flight when the abort landed). This makes `controller.abort()` from inside a bridge handler a spoof-proof way to stop a run — sandbox code cannot catch or swallow it.
  - Leaves the pool healthy: the torn-down slot is replaced with a fresh connection, so other runs and subsequent runs are unaffected.

  Note: aborting a purely CPU-bound run resolves the `run()` promise immediately, but the abandoned isolate is only reclaimed when its CPU guard fires (bounded by `cpuTimeMs`, not `wallTimeMs`).

- 91fc138: feat: propagate JS error name and structured data across the sandbox bridge

  `RunError.name` now reflects the actual JS error name (`TypeError`, `RangeError`, custom names via `error.name = '…'`) instead of always being `'Error'`. For `ERR_USER_CODE` errors, a new optional `data` field carries the thrown error's own enumerable properties (excluding `name`, `message`, `stack`) serialised via the same WireValue path as exports — functions and other non-serialisable values are silently dropped.

  Non-object throws (`throw 'string'`, `throw 42`) keep `name: 'Error'` and `data: undefined`.

## 0.0.4

### Patch Changes

- fbe4332: feat: import support

## 0.0.3

## 0.0.2

### Patch Changes

- 1d49cd7: chore: adjust binary peer dep version

## 0.0.1

### Patch Changes

- 88554a4: initial release
