# @iso4/sandbox

## 0.3.1

### Patch Changes

- e7e0004: `result.durationMs` now has microsecond resolution (e.g. `0.347`) instead of being truncated to whole milliseconds. Sub-millisecond runs previously reported `0`.
- 7ee7e57: Exported `Uint8Array` values now arrive on the host as a real `Uint8Array` instead of an index-keyed object. Unsupported types (`Date`, `Map`, `Set`, `RegExp`, `ArrayBuffer`, typed arrays other than `Uint8Array`, class instances) now fail with a clear error in both directions instead of silently turning into `{}`. `Date` was also removed from host-module data leaves so what goes in matches what can come back out.

## 0.3.0

### Minor Changes

- f25cbd8: Add async context propagation inside the sandbox via a minimal, Node-compatible `AsyncLocalStorage`, imported the standard way:

  ```js
  import { AsyncLocalStorage } from "node:async_hooks";
  const als = new AsyncLocalStorage();
  await als.run(store, async () => {
    /* ... */ als.getStore();
  });
  ```

  Sandboxed run/postfix code can now carry an ambient value across `await` points — a trace id, a durable-workflow step key — without threading it through every call, and concurrent async chains stay isolated (a module-level variable gets this wrong). The canonical use case is a `step.do(name, fn)` shim whose nested-step key is built from an `AsyncLocalStorage`, so a step nested inside another produces `parent/child` and never collides.

  - Only `run(store, callback, ...args)` and `getStore()` are implemented — the concurrency-safe core.
  - Built on V8's continuation-preserved embedder data (the same primitive modern Node's `AsyncContextFrame` uses). No promise hooks are registered, so runs that never use it pay nothing; when used, the small per-`await` cost bills to `cpuTimeMs`/`wallTimeMs`, never to the bridge-call budget.
  - Always available to run code with no host opt-in, like `console`. **Not** available in `precompile()` (prefix) code — the native bindings can't be captured in a V8 startup snapshot. See DESIGN.md §16.
  - `node:async_hooks` is the sole runtime-provided `node:*` module; a host-declared import of the same specifier takes precedence.

  The `@iso4/v8-*` native binaries and `@iso4/sandbox` are version-fixed and release together.

### Patch Changes

- f25cbd8: Fix a crash where `precompile()` with an unresolvable import (or otherwise un-instantiable prefix module) could segfault the runtime process instead of returning an error. Precompile now validates the prefix in a throwaway isolate before building the snapshot, so a bad prefix — syntax error, unresolved import (including `node:async_hooks`, which is intentionally not available in prefix code), or throwing top-level code — fails cleanly with the appropriate error (e.g. `ERR_MODULE_NOT_FOUND`).

## 0.2.2

### Patch Changes

- 9958d76: Restore thrown-error shape across the bridge: direct props in the sandbox, `error.fields` on the host.

  - **Host → sandbox:** a host handler error's own-enumerable properties (e.g. `status`, `reason`, a custom `reasoning`) are now re-attached as **direct own properties** on the Error the sandbox catches — `e.status` instead of the previous `e.data.status`. Reserved keys (`name`/`message`/`stack`/`__proto__`) can never be injected through the payload. Rethrowing the caught error (or spreading it into a fresh one) round-trips all fields to the host.
  - **Sandbox → host (breaking rename, pre-1.0):** `RunResult.error.data` is now `RunResult.error.fields`, typed `Record<string, unknown>` — a record of _all_ extra own-enumerable props, so a thrown error's own `data` property lands as `fields.data` and nothing can collide with `error.code`. `stack` stays a dedicated top-level `stack?: string`; `name`/`message`/`stack` are reserved and never appear inside `fields`.
  - **Thrown primitives fixed:** `throw "some string"` in the sandbox now produces a clean `{name: 'Error', message: 'some string'}` — previously it carried a literal `"undefined"` stack and the string's character indices as data.

## 0.2.1

### Patch Changes

- 00f072d: Propagate host-handler error `name`, `message`, and `data` across the bridge (#22).

  When a host global/import handler throws, the sandbox now receives a real `Error` whose `name` matches the thrown error (built-in names like `TypeError` use the matching constructor, so `instanceof` works), with own-enumerable properties beyond `name`/`message`/`stack` carried as `e.data`. The host **stack is deliberately never sent** into the sandbox — it can expose host file paths and infrastructure details.

  Behavior changes (breaking, pre-1.0):

  - **Host handler errors are now catchable in the sandbox.** Previously any handler error terminated the run even when sandbox code caught it. Uncaught handler errors still fail the run with `ERR_HOST_BRIDGE`, now with `name`/`message`/`data` preserved on `RunResult.error` instead of the flattened `name: 'Error'`. Bridge limit violations (`maxBridgeCalls`, `maxBridgeCallBytes`, function arguments) remain fatal.
  - The `BridgeResponse` wire format gained an error `data` field (protocol version stays at 1 pre-1.0). `@iso4/sandbox` and the `@iso4/v8-*` binaries must be upgraded together — they are version-fixed and release in lockstep.

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
