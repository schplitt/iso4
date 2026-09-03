# @iso4/sandbox

## 0.5.0

### Minor Changes

- 2f90212: feat: runs share connections, and a run's slot frees at its Result (#127)

  Concurrent runs are multiplexed onto shared connections (several per
  socket, routed by run id) instead of opening one connection each, and a run
  with pending `waitUntil` work no longer holds admission capacity during its
  background grace phase. `maxConcurrentRuns` semantics are unchanged;
  `SandboxStats.openConnections` now tracks roughly peak concurrency divided
  by the per-connection share instead of peak concurrency.

- c6c7d13: chore: bump the v8 crate 130 → 147 (V8 13.0 → 14.7)

  The serialization format is unchanged, so `@iso4/sandbox` and `@iso4/v8-*`
  still pair exactly as before. Most native paths are 10–47 % faster.

- 344d259: feat: capacity manager — memory budget and `sandbox.stats()` (#65)

  New `memoryBudgetMb` decides how many isolates stay alive, container-aware by
  default, while `maxIsolates` still caps concurrent runs. `sandbox.stats()`
  returns a capacity snapshot over a control connection outside the run pool.

- 16cce13: feat: memory capacity rails — global-container metering, a hard admission line, and bounded queueing (#77)

  Memory watermarks now measure the whole container (cgroup working set,
  Node host included) instead of the runtime child alone, the budget default
  becomes 80% of the container limit minus a 256 MB host reserve, and a new
  isolate is never created when measured usage plus the run's `memoryMb`
  would cross 90% of that base — such runs fail with the new `ERR_CAPACITY`
  code instead of queueing. `maxConcurrentRuns`' automatic default is now
  memory-bounded, and the new `maxQueuedRuns` (default 100 × slots) sheds
  callers past the queue bound with the new `ERR_QUEUE_FULL` code.

- a5a6739: fix: connection integrity and result correlation (#73)

  Every `Result` is matched against the run that asked for it, and a connection
  whose frame alignment is in doubt is replaced rather than reused. `maxIsolates`
  is now a capacity rather than a fixed set of connections, and there is a new
  host-detected error code `ERR_PROTOCOL_DESYNC`.

- e074119: feat: RSS watermark + scored eviction — `heapUsed × idleTime` (#66)

  The memory budget is enforced against the runtime's own process RSS: at the
  mark, idle warm instances are evicted by score and new warm admissions stop
  until RSS falls back to 80 % of it. `sandbox.stats()` gains `budgetBytes`,
  `rssBytes` and `underPressure`; the instance-count cap is gone.

- f962a14: Freeze the sandbox clock during execution, workerd-style. `Date.now()`, no-arg `new Date()`/`Date()`, no-arg `Intl.DateTimeFormat` formatting, and `Temporal.Now.*` all read one per-context value that advances — monotone, whole milliseconds — only when the runtime regains control at run entry, a bridge response, or a stream frame. Sandboxed code can no longer observe its own elapsed execution time, closing the timing side-channel between co-resident isolates. Explicit-argument `Date`/`Temporal`/`Intl` computation is untouched. Alongside it, `SharedArrayBuffer` is removed from the sandbox global (as in non-cross-origin-isolated browsers; `Atomics` on plain buffers keeps working) and `Atomics.wait` now throws, matching workerd.
- b45658c: feat: host → sandbox function calls — `prefix.call({ export, args })` and `run({ code, call })` (#58)

  Call a function that already lives in the sandbox, addressed by export path,
  with arguments crossing as one V8 blob. An export that cannot cross no longer
  fails a plain run — it is reported in the new `skippedExports` instead.

- 71b3f26: feat: native `setTimeout`/`clearTimeout` in run code (#79)

  Timers run natively in the runtime's event loop: a sleep costs no CPU budget but counts against the wall budget, `Date.now()` advances by exactly the requested delay, and a run's pending timers die with it. Both names are now reserved global names; `setInterval` is deliberately not provided.

- ec9e042: feat: per-prefix and per-one-off heap caps (#77)

  `prepare({ memoryMb })` caps every isolate serving that prefix, and one-off
  `run()` accepts `limits.memoryMb` again for its fresh isolate — the
  sandbox-level `memoryMb` becomes the default for both. Prefix
  `execute()`/`call()` still reject a per-run value: warm isolates are shared
  and their cap is fixed at creation.

- f49f2b2: feat: prefix runs join busy instances; instances scale with measured CPU demand (#77)

  A prefix run with no idle instance now joins a busy one (the engine
  interleaves runs) instead of always cold-starting, and another isolate is
  opened only when the prefix's measured CPU demand justifies it — so
  waiting-heavy traffic stops piling up isolates while compute-heavy traffic
  still scales out for response time. `SandboxStats.activeRuns` now counts
  runs (several can share one instance); `warmInstances` counts instances as
  before.

- 911a82d: refactor: remove runtime V8 snapshot creation — prefixes are validated source, re-evaluated per run (#60, #61, #62)

  The public API and the wire protocol are unchanged, but per-run latency now
  includes prefix evaluation, and a nondeterministic prefix produces per-run
  values. Closes the intermittent child-process crash under concurrent
  `prepare()`.

- bbcd205: feat: run-slot admission, lazy connections, and a per-run frame router (#126)

  Breaking: `maxIsolates` is replaced by `maxConcurrentRuns` (same default) — it caps runs executing at once, the rest queue FIFO, and `AbortSignal.timeout()` bounds the wait. Connections to the runtime now open on demand instead of all at `createSandbox()`, and `stats()` gains `openConnections`.

- 139386b: feat: session demux and per-instance turn loop (#125)

  Runs no longer hold a thread while suspended on host calls, and a waiting
  run's abort or timeout now fails that run alone instead of evicting its
  warm instance. New error code
  `ERR_INSTANCE_RESET` (with `resetCause` and `culpritRunId` on the error)
  reports runs that were in flight on a shared instance when a co-resident run
  had to be terminated mid-execution.

- cf851bf: feat: sandbox web runtime — `Headers`, `Request`, `Response`, `URL`, `TextEncoder` and friends

  `Request`, `Response` and `Headers` cross the boundary as real instances rather
  than flattening to plain objects. New error code `ERR_TYPE_NOT_SERIALIZABLE`
  for values that cannot cross; streams are deliberately unsupported.

- e1b10fc: feat: upgrade to V8 15.2 and span Node 22–27 with one binary (#80)

  The runtime picks up five V8 release lines of security fixes and performance
  work. Serialized values keep the format every shipping Node reads, while the
  handshake now also accepts hosts on newer Node lines that write V8's
  next serialization format — so Node 22 through 27 pair with the same binary.

- 467479f: feat!: replace the WireValue codec with V8 serialization blobs

  Protocol version 1 → 2, so `@iso4/sandbox` and `@iso4/v8-*` must be updated
  together and a mismatch now fails at `createSandbox()`. `Date`, `Map`, `Set`,
  `RegExp`, `Error`, typed arrays and cycles round-trip as real instances, and
  dense payloads are ~5.9× faster.

- b2a19f8: feat: warm isolate registry — prefix runs reuse resident isolates (#64)

  Warmth is a cache and never a guarantee: module-scope state may survive between
  runs on an instance and may be evicted at any time. Breaking — `limits.memoryMb`
  moves to `createSandbox({ memoryMb })` with the default raised to 128 MB; new
  `ERR_WARMUP_LIMIT` and a new `heapUsedBytes` result field.

- ea8937f: feat: widen `HostExportData` to everything V8 serialization carries

  `Date`, `RegExp`, `Error`, `Map`, `Set`, `ArrayBuffer`, typed arrays, `DataView`
  and cycles now cross as real instances. Host-module data leaves are no longer
  inspected at registration, so an unsupported value fails with the serializer's
  own error rather than one naming the exact leaf path.

### Patch Changes

- fd462aa: perf: cheaper per-call attribution for bridge calls and console output (#127)

  Native callbacks resolve the owning run in one table lookup and take a
  reference-counted handle to the stub binding instead of cloning it per
  invocation. Attribution semantics are unchanged.

- f1a2e24: fix: Request/Response clone() no longer shares its body buffer

  A cloned Request or Response now gets its own copy of a buffer body, so
  mutating one side's bytes no longer reaches through to the other.

- 17a87e7: feat: disable eval and new Function in run code

  Code generation from strings is now a prepare()-time capability: setup code can still compile functions from strings, but per-run code calling `eval` or `new Function` gets a catchable `EvalError` and the run continues.

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

- 5ca5ab9: perf: receive large results without stalling the host

  Incoming chunks are joined once per frame instead of re-concatenated per
  chunk. A 15 MB result now takes ~19 ms rather than ~1.2 s.

- f186aec: perf: the frozen-clock advance no longer allocates V8 state per turn (#127)

  Clock handles are cached per isolate at install time; a turn within the
  same wall millisecond now skips V8 entirely. Clock semantics are unchanged.

- 5a5cfbe: fix: reject a supplied-but-malformed per-run global override

  A per-run `globals` override that is present but not a function (e.g. a tenant
  handler that resolves to `undefined`) now throws instead of silently falling
  back to the precompile-time default, matching the imports side.

- 03b9e46: perf: one shared watchdog thread now enforces all CPU/wall budgets (#145)

  Warm instances hold one OS thread instead of two, so deployments with hard
  per-container thread limits can keep roughly twice as many instances warm.
  Budget and timeout semantics are unchanged.

- dbd1caf: perf: instance turn loops route run events through one channel and a deadline heap (#127)

  Frame routing on a busy instance no longer scales with the number of
  in-flight runs, and boundary deadlines fire in arrival order: co-resident
  frame traffic can neither starve a run's wall timeout nor turn a run whose
  answer arrived in time into one.

- 768d839: fix: install runtime-internal globals non-enumerable

  The `__iso4_*` plumbing no longer appears in `Object.keys(globalThis)` or `for...in`; host-declared globals stay enumerable like browser globals. Sandbox code that enumerates its environment no longer trips over runtime internals.

- 5ca5ab9: fix: stop the runtime when the host exits without `dispose()`

  A last-resort `exit` hook now ends it. `dispose()` is still the only complete
  answer, since a host killed by a signal runs no JavaScript.

- f6b13f8: fix: a bridge handler settling after its run completed releases its streams (#127)

  A streamed body returned by a handler whose run already finished no longer
  leaves its host-side reader locked; the source is released since nothing
  will ever pump it.

- 3637971: fix: WHATWG-compliant URL in the sandbox

  The sandbox `URL` is now backed natively by the ada parser (the one Node.js
  uses) and passes the WPT URL test suite, including IDNA, relative resolution
  and non-special schemes. `URL.parse`, `URL.canParse` and the previously
  missing component setters are now available.

- 926a690: feat: the runtime child marks itself as the preferred OOM victim (#77)

  On Linux the spawned runtime raises its own `oom_score_adj` at startup, so
  a container out-of-memory kill takes the sandbox child (failed runs, a
  respawnable sandbox) instead of the Node host.

- 7aec3d4: perf: outbound frames skip the writer thread when the socket is free (#127)

  Removes the flat per-call overhead the bounded outbound queue introduced
  for short, frequent calls; the writer thread still takes over whenever the
  socket backs up, so stall isolation is unchanged.

- a1f6e4a: feat: per-global opt-out of enumerability

  The object global forms accept `enumerable: false` to keep an injected global out of `for...in` / `Object.keys` while staying callable, and new `{ kind: 'bridge', handler }` / `{ kind: 'string', expr }` object forms carry the option for what the shorthands declare. Shorthand globals stay enumerable.

- 2e6fb0f: fix: an abort that arrives before its run starts still lands gracefully (#127)

  A Terminate (or connection loss) racing ahead of the run's dispatch is now
  remembered and answered when the run arrives, instead of falling back to
  the host-side teardown timeout.

- 1925209: fix: top-level `await` in prefix code no longer fails `prepare()` (#55)

  The runtime now drains the microtask queue until the prefix's evaluation
  promise settles. Two new error codes state the remaining limits:
  `ERR_PREFIX_BRIDGE_CALL` and `ERR_PREFIX_DID_NOT_SETTLE`.

- 1ed87b3: fix: identify runtime web types by an internal construction-time tag

  Serializing a `Headers`, `Request` or `Response` no longer depends on the classes being intact on `globalThis`, and overriding `Symbol.hasInstance` can no longer change how an instance crosses the boundary.

- c0de1ab: fix: preserve an own `__proto__` key crossing into the sandbox

  Rebuilding a host-supplied plain object no longer triggers the prototype setter,
  so an own-enumerable `__proto__` key (e.g. from `JSON.parse`) crosses as data
  instead of being silently dropped.

- cf037f8: perf: cut fixed per-run overhead — ~4 % hot-run latency, ~6 % throughput

  Per-run trace logs are off by default; set `ISO4_V8_TRACE=1` to restore them.
  Prefix snapshots are shared by handle instead of copied twice per run.

- 9819e84: fix: reject URL credentials and control-char statusText in the sandbox

  `new Request(url)` now throws when the URL includes credentials (fetch spec),
  and `new Response` rejects a `statusText` containing control characters
  (mirroring workerd) — so these fail on the user's line rather than host-side.

- 51e824d: fix: keep resource limits armed through result serialization

  Serializing a run's result executes guest getters, so it now stays under the
  run's CPU, wall and memory budgets; serialization time counts against
  `cpuTimeMs`/`wallTimeMs`.

- 6c8c0da: fix: rehydrate only host-emitted host-type descriptors, stamped per session

  Host-to-sandbox `Headers`/`Request`/`Response` descriptors now carry a random per-sandbox stamp negotiated at connection setup, and the runtime rebuilds only stamped descriptors. Structured data passed into a run can no longer be reinterpreted as a host type, and no property name is reserved anymore.

- 343da7a: fix: replace the argv auth token with a kernel-enforced private socket directory

  The runtime socket now lives in a fresh owner-only (0700) per-sandbox directory, so access is enforced by the kernel at connect time. The token is gone from the spawn args and the wire handshake (`@iso4/sandbox` and `@iso4/v8-*` are released in lockstep).

- 3504569: fix: an instance thread that fails to spawn fails only its own run (#127)

  Under process resource exhaustion the affected run now reports
  ERR_INTERNAL and the connection keeps serving, instead of the runtime
  panicking the connection's demux.

- 5ca5ab9: fix: clean up the runtime process when `createSandbox()` fails

  A failed startup left the runtime running with no `Sandbox` to dispose it. A
  runtime that exits during startup is now reported with its exit code instead
  of a socket timeout.

- 985e9b6: fix: stream frames must carry a real run id (#127)

  An unattributed stream frame (run id 0) now tears the connection down as a
  protocol desync instead of being matched to a run by stream id — with
  several runs on one connection that guess could credit or cancel the wrong
  run's body stream. The production runtime always tags stream frames.

- 6cb167b: feat: stream large Request/Response bodies into the sandbox

  A host body that outgrows a 64 KiB probe now crosses as a stream pumped under flow control instead of being buffered whole: lower memory, and the sandbox starts reading on the first chunk via `.body` or the body helpers. Small bodies keep the buffered path unchanged; returning a streamed body to the host still requires reading it first.

- ba357d0: fix: identify runtime web types independently of one another during serialization

  Serializing a `Headers`, `Request` or `Response` no longer fails just because sandbox code removed or shadowed one of the other classes on `globalThis`.

- 3586fcd: feat: `waitUntil()` lets a run finish background work after its result

  Sandbox code can register background work with the new `waitUntil(promise)` global (also importable from `iso4:runtime`); the caller gets the result immediately and the work keeps running up to a configurable grace budget (`limits.graceMs`, default 30 s). The outcome arrives as `result.waitUntil`, a never-rejecting promise with status and telemetry.

## 0.4.1

### Patch Changes

- b51d9cd: Rename `sandbox.precompile()` → `sandbox.prepare()` and `prefix.run()` → `prefix.execute()`. The new names are the canonical API; the former names remain as **deprecated aliases** with identical behavior (they delegate to the same implementation) and are slated for removal in a future major. No behavior change — existing code keeps working. `Sandbox.prepare` and `Prefix.execute` are first-class members of the public type surface; `precompile`/`run` carry `@deprecated` JSDoc.

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
