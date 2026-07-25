# @iso4/sandbox

## 0.4.0

### Minor Changes

- 4126420: Every `RunResult` now reports what the run did, measured inside the Rust runtime: `cpuTimeMs` (active V8 execution time, bridge waits excluded — `durationMs` remains wall-clock time) and `bridgeCalls`, one metadata record per bridge call attempt in attempt order — resolved name (`fetch`, `tools:search.query`), start offset and round-trip duration on the run's clock, argument/response byte sizes, and outcome. Attempts blocked by `maxBridgeCalls`/`maxBridgeCallBytes` are included with `blocked: true`, so runs that hit a limit show exactly which attempt crossed it. Payloads are never captured, keeping the report cheap enough to stay always-on. Aborted runs report zeros and an empty list for now (the abort teardown precedes the runtime's result frame; see #36).
- 9332cca: Aborting a run now terminates it gracefully whenever possible, so the aborted `RunResult` carries real telemetry instead of synthesized zeros. Previously an abort destroyed the socket and TypeScript fabricated the result (`durationMs: 0`, `cpuTimeMs: 0`, `bridgeCalls: []`) — which meant every durable-isolates suspension (implemented as an abort) lost its per-call bridge records. Now TypeScript sends a `Terminate` frame carrying the run ID; the Rust runtime, parked in its bridge-wait poll loop, stops the run and replies with a real `ERR_ABORTED` result reporting `durationMs`, `cpuTimeMs`, and the bridge-call records collected up to the abort. The connection stays healthy and is reused rather than reconnected. This covers the case that matters most — suspensions always happen while the sandbox is awaiting a bridge call, exactly when the runtime can receive the frame. A run stuck in a tight synchronous loop can't be reached this way, so after a short grace period TypeScript falls back to today's socket teardown and synthesized (zeroed) result; the busy isolate is still reclaimed by its CPU guard.
- 0fee306: Host-import modules are now built natively by the Rust runtime from structured shape data instead of being lowered to generated JavaScript source in the client. Previously the client walked a host-module object and emitted ESM text — data leaves printed as JS literals, function leaves as `__iso4_call(<id>, …)` stubs — an injection-adjacent codegen surface, and the runtime only ever saw opaque source. The module shape now crosses the wire as plain data (function-leaf markers, data leaves as `WireValue`s, nested objects as trees); the runtime materialises data leaves with the value codec, builds function leaves as async trampolines from a fixed factory (the handle ID passed as a number, never printed into source), and hands the values to a fixed-shape module through V8's `import.meta` callback. Host modules remain ordinary source-text modules, so prefix snapshots capture their bindings as plain JS exactly as before.

  The runtime owns the handle → `<specifier>.<path>` table, so `RunResult.bridgeCalls` records now arrive with **fully resolved names** for host-import calls (e.g. `tools:search.query`) straight from the runtime — the client-side name resolver is gone — and `BridgeCall` frames carry the resolved import target (`targetKind: import` + specifier + leaf path) instead of a dispatcher name and a numeric handle argument.

  Undeclared-import rebind validation moved into the runtime: `prefix.run()` sends only the rebind locations, and the runtime checks them against the shape declared at `precompile()`, rejecting undeclared specifiers/paths, data leaves, and source modules with `ERR_UNDECLARED_BINDING` — the same enforcement point that guards undeclared globals, no longer bypassable by a non-TypeScript client. Compile-time enforcement via `RebindImports<M>` is unchanged.

  Wire-protocol change (`ImportBinding` is now tagged source/host with a shape tree; `PrefixRun` carries `ImportRebind` locations; `BridgeCallRecord` carries a resolved `name` and drops `rawName`/`importHandleId`): `@iso4/sandbox` and the `@iso4/v8-*` runtime binaries must be released together.

- 2f0e296: Host globals are now installed natively by the Rust runtime instead of by prepending generated JavaScript to the user's code. Previously `processGlobals` turned string globals and `BridgeWithShim` wrappers into source text and pasted it in front of the run's code, which shifted every line of user code so sandbox stack traces pointed at the wrong lines, and interpolated the global's name into an identifier position in the generated wrapper.

  The client now sends each global as structured data — a `GlobalDef` tagged `bridge`, `string`, `data`, or `shim` — and the runtime installs it directly on the sandbox global object via the V8 API. String expressions and shim wrappers are evaluated as their own scripts with their own filenames, so **user code always starts at line 1 and its stack traces are correct**, and a global's name only ever travels as a string passed to `object.set` (or a `WireValue`) — never interpolated into code.

  Adds a new **data-valued global kind** for passing a plain constant: `globals: { config: { kind: 'data', value: … } }`. The value crosses the wire as a `WireValue` (same supported set as host-module data leaves — primitives, `bigint`, `string`, `Uint8Array`, plain objects/arrays) and is materialised natively, removing the need to hand-roll a `JSON.stringify`-into-a-string-expression global. Data globals, like string globals, are constants and cannot be rebound per `prefix.run()`.

  For precompiled prefixes, string/data globals and shim wrappers are baked into the snapshot at `precompile()` time exactly as before, so only bridge stubs are re-installed per `prefix.run()` — the repeated-run hot path is unchanged.

  Wire-protocol change (the `globals` field of `Run`/`Precompile`/`PrefixRun` is now a `List<GlobalDef>` instead of a name list): `@iso4/sandbox` and the `@iso4/v8-*` runtime binaries must be released together.

- f1ccb9d: The Rust runtime now owns the resource-limit defaults; the client sends only the limits the caller explicitly set. Previously the default numbers (64 MB memory, 5 s CPU, 30 s wall, 10 bridge calls, 16 MiB export/bridge-payload caps, 1 MiB stdio caps) were filled in client-side before every Run/Precompile/PrefixRun and shipped as concrete values on the wire, so the same constants were documented in three places (TS code, `types.ts` jsdoc, Rust doc comments) that could drift, and any non-TS client had to re-implement them to get safe behavior.

  Each `ResourceLimits` field is now `Optional<u32>` on the wire: absent means "apply the runtime default", an explicit `0` still means "no limit" (distinct from absent). Rust resolves any absent field from a single set of `DEFAULT_*` constants in `native/v8-runtime/src/ipc.rs` — the source of truth. The public `ResourceLimits` fields become optional to match (`limits` is `ResourceLimits` rather than `Partial<ResourceLimits>` at the call sites), and their `@default` jsdoc now documents the runtime defaults it mirrors. Effective behavior for existing callers is unchanged; the defaults are identical.

  Wire-protocol change (limits payload shape): `@iso4/sandbox` and the `@iso4/v8-*` runtime binaries must be released together.

### Patch Changes

- 91e6b59: Bridge limit violations (`maxBridgeCalls`, `maxBridgeCallBytes`, function arguments) now terminate V8 execution immediately and uncatchably, as DESIGN.md always specified. Previously they were thrown as catchable JS exceptions: sandbox code could `try/catch` past a violation in the synchronous window before the next microtask checkpoint, keep making (blocked) call attempts, or even complete the run successfully despite the violation. Host handler errors are unchanged and remain catchable in the sandbox.

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
