# @iso4/v8-linux-arm64-gnu

## 0.5.0

### Minor Changes

- f962a14: Freeze the sandbox clock during execution, workerd-style. `Date.now()`, no-arg `new Date()`/`Date()`, no-arg `Intl.DateTimeFormat` formatting, and `Temporal.Now.*` all read one per-context value that advances — monotone, whole milliseconds — only when the runtime regains control at run entry, a bridge response, or a stream frame. Sandboxed code can no longer observe its own elapsed execution time, closing the timing side-channel between co-resident isolates. Explicit-argument `Date`/`Temporal`/`Intl` computation is untouched. Alongside it, `SharedArrayBuffer` is removed from the sandbox global (as in non-cross-origin-isolated browsers; `Atomics` on plain buffers keeps working) and `Atomics.wait` now throws, matching workerd.
- 139386b: feat: session demux and per-instance turn loop (#125)

  Runs no longer hold a thread while suspended on host calls, and a waiting
  run's abort or timeout now fails that run alone instead of evicting its
  warm instance. New error code
  `ERR_INSTANCE_RESET` (with `resetCause` and `culpritRunId` on the error)
  reports runs that were in flight on a shared instance when a co-resident run
  had to be terminated mid-execution.

- e1b10fc: feat: upgrade to V8 15.2 and span Node 22–27 with one binary (#80)

  The runtime picks up five V8 release lines of security fixes and performance
  work. Serialized values keep the format every shipping Node reads, while the
  handshake now also accepts hosts on newer Node lines that write V8's
  next serialization format — so Node 22 through 27 pair with the same binary.

### Patch Changes

- fd462aa: perf: cheaper per-call attribution for bridge calls and console output (#127)

  Native callbacks resolve the owning run in one table lookup and take a
  reference-counted handle to the stub binding instead of cloning it per
  invocation. Attribution semantics are unchanged.

- f6cddb7: fix: a host that stops draining a connection can no longer stall sandbox execution (#127)

  Outbound frames now go through a bounded per-connection queue with a
  dedicated writer thread; a peer that stops reading fails that connection's
  runs cleanly after a bounded wait instead of freezing every instance that
  shares it.

- 61e420c: Embed ICU data in the runtime binary. Locale-aware calls in sandboxed code (`toLocaleString`, `Intl.*`, `localeCompare`) previously aborted the whole V8 runtime process with "Fatal process out of memory: DateTimePatternGeneratorCache::CreateGenerator", leaving the sandbox unreachable for every subsequent run. They now return correctly localized output.
- 8871645: fix: inbound frames are read against the flat protocol ceiling (#127)

  Per-run frame allowances are still enforced per run, but the connection's
  read ceiling is now a constant — it can no longer shrink under an in-flight
  frame, so a large late frame for a just-completed run is discarded instead
  of costing the connection.

- f186aec: perf: the frozen-clock advance no longer allocates V8 state per turn (#127)

  Clock handles are cached per isolate at install time; a turn within the
  same wall millisecond now skips V8 entirely. Clock semantics are unchanged.

- 03b9e46: perf: one shared watchdog thread now enforces all CPU/wall budgets (#145)

  Warm instances hold one OS thread instead of two, so deployments with hard
  per-container thread limits can keep roughly twice as many instances warm.
  Budget and timeout semantics are unchanged.

- dbd1caf: perf: instance turn loops route run events through one channel and a deadline heap (#127)

  Frame routing on a busy instance no longer scales with the number of
  in-flight runs, and boundary deadlines fire in arrival order: co-resident
  frame traffic can neither starve a run's wall timeout nor turn a run whose
  answer arrived in time into one.

- 7aec3d4: perf: outbound frames skip the writer thread when the socket is free (#127)

  Removes the flat per-call overhead the bounded outbound queue introduced
  for short, frequent calls; the writer thread still takes over whenever the
  socket backs up, so stall isolation is unchanged.

- 2e6fb0f: fix: an abort that arrives before its run starts still lands gracefully (#127)

  A Terminate (or connection loss) racing ahead of the run's dispatch is now
  remembered and answered when the run arrives, instead of falling back to
  the host-side teardown timeout.

- 3504569: fix: an instance thread that fails to spawn fails only its own run (#127)

  Under process resource exhaustion the affected run now reports
  ERR_INTERNAL and the connection keeps serving, instead of the runtime
  panicking the connection's demux.

## 0.4.1

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

## 0.1.0

### Minor Changes

- 91fc138: feat: propagate JS error name and structured data across the sandbox bridge

  `RunError.name` now reflects the actual JS error name (`TypeError`, `RangeError`, custom names via `error.name = '…'`) instead of always being `'Error'`. For `ERR_USER_CODE` errors, a new optional `data` field carries the thrown error's own enumerable properties (excluding `name`, `message`, `stack`) serialised via the same WireValue path as exports — functions and other non-serialisable values are silently dropped.

  Non-object throws (`throw 'string'`, `throw 42`) keep `name: 'Error'` and `data: undefined`.

## 0.0.4

### Patch Changes

- fbe4332: feat: import support

## 0.0.3

### Patch Changes

- 7c943af: chore: add bin field to native packages

## 0.0.2

### Patch Changes

- 58c27ad: ci: set execute bit on native binaries

## 0.0.1

### Patch Changes

- 88554a4: initial release
