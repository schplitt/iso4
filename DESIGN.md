# iso4 — Design

A small, sharp V8-isolate sandbox for running untrusted JavaScript with
controllable memory and execution-time limits, host-curated network access,
and a clean module-resolution API. Successor to the secure-exec idea, but with
none of the Node-compatibility surface area.

---

## 1. Goals

The execution model is always the same:

```
precompile prefix once  →  call a function with some input  →  get a value back
```

The prefix is a V8 startup snapshot that captures compiled libraries, tool
bindings, and helper code. Every execution restores from that snapshot (or
reuses a live isolate from it) and runs a function, then returns its result.
The function body and the input differ between deployments; the machinery
is identical.

Two independent axes control the trade-off:

### 1.1 Code variability

**Dynamic code** — the function body is provided at call time and changes
per call. The caller (an AI agent, an MCP client, a script host) provides
an async arrow function body; the host wraps it before running:

```js
// What the caller provides:
async () => {
  const data = await fetchTool('search', query)
  return data.results
}

// What the host runs:
export default await (async () => {
  const data = await fetchTool('search', query)
  return data.results
})()
```

`prefix.run({ code })` handles this. A fresh isolate is created from the
snapshot per call so each caller starts from a clean state regardless of
what previous callers did. Multiple callers running simultaneously each get
their own pool slot and run in parallel.

**Static code** — the function is compiled into the prefix and does not
change between calls. Only the input data varies. The prefix exports the
function by name; the host calls it with each new input:

```js
// Prefix code (compiled once into the snapshot)
export async function transform(row) {
  return { revenue: row.price * row.qty }
}
```

`session.call('transform', row)` handles this (Phase 11). The isolate is
reused across calls: no recompilation, no snapshot restore per call. In v1
before Phase 11, the same result can be achieved through `prefix.run()` with
a per-run host import that provides the input data, at the cost of snapshot
restore overhead per call.

Both modes use the same `PrecompiledPrefix`. The prefix author declares
tools, globals, and libraries once. Dynamic vs static is a decision at
call time, not at precompile time.

### 1.2 Isolation level

**Two-process backend** (default, v1): the V8 isolate runs in a separate
Rust process communicating over UDS. A crash or OOM in the isolate kills
the subprocess; the host process and all other concurrent runs are
unaffected. Adds ~30–100 µs per bridge call. Right for any deployment
where the code is not fully trusted or where crash isolation matters.

**In-process backend** (Phase 12, opt-in): the V8 isolate runs inside the
Node process via a C++ NAPI addon. No IPC overhead per call. An OOM can
crash the host process. Requires Docker/Kubernetes (or equivalent) as the
outer security boundary. Selected with `RuntimeOptions.backend: 'inprocess'`.

The two axes are independent: static code with two-process backend works
fine at moderate throughput; dynamic code with in-process backend is valid
for trusted-code deployments. The two-process backend is always the default
because it requires no external security infrastructure.

### 1.3 In-scope goals

1. **Pure JavaScript execution** in V8 isolates, one isolate per run/session.
2. **Memory limit** enforced at the V8 heap and external `ArrayBuffer` level.
3. **CPU time limit** that excludes time spent waiting on host async work
   (so `await fetch(...)` doesn't burn the budget).
4. **Hard wall-clock cap** as a backstop.
5. **Host-supplied `fetch`** with a per-call permission predicate. The library
   consumer decides which requests are allowed. Hardened defaults ship as
   a separate package (`@iso4/fetch`) so the core stays minimal.
6. **Host-supplied module resolver** with two flavors:
   - **Source modules**: the host provides JS source for a specifier; V8
     compiles it once, runs it in-isolate. Cached via V8 code cache.
   - **Host-implemented modules**: the host provides an object of functions;
     calls from the sandbox cross a bridge to the host and return data.
7. **Result extraction via ESM `export`**. User code is always wrapped so
   its return value becomes `export default`. Results cross the boundary
   through V8 `ValueSerializer`, which naturally restricts results to plain
   data (no functions, no methods).
8. **Captured stdout/stderr** via a runtime-owned `console`.
9. **Pre-compilable prefix code** via V8 startup snapshots. Snapshotting
   the prefix once cuts cold start from ~30–80 ms to ~2–5 ms steady-state.
10. **Composable monorepo structure**: the runtime is one package (`iso4`);
    fetch hardening (`@iso4/fetch`), stdlib stubs (`@iso4/fs`,
    `@iso4/crypto`, …), and future extensions are siblings. Each one is a
    small factory that produces a `HostImport` or `FetchHandler` ready to
    plug into the core. See `MONOREPO.md`.

Explicitly **out of scope**:

- Any Node.js standard library emulation (`fs`, `http`, `process`, `Buffer`,
  `child_process`, `crypto.createHash`, etc.). If the host wants those, the
  host implements them via `imports`.
- Callbacks crossing the sandbox boundary in either direction. No
  `setTimeout(fn)` shipped by the host, no event listeners on host objects.
  Function values cannot cross — only data. (Tier 2 may add a callback-handle
  table; not needed for v1.)
- Streaming `Response` / `ReadableStream`. v1 buffers full bodies host-side
  with a configurable cap.
- Sharing live state between runs. Each `prefix.run()` is a fresh isolate
  booted from the prefix snapshot — the _prefix's heap shape_ is shared,
  not live mutable state.
- TypeScript / JSX compilation. Host's responsibility to ship pre-compiled JS.
- POSIX surface (sockets, FDs, pipes, PTY, process tables). None of it.

The intentional consequence: a sandbox where every interaction with the
outside world is explicit, named, and host-mediated. Nothing leaks unless the
host hands it over by name.

---

## 2. Architecture (the "what runs where")

```
                                                                
  Host process (Node/Bun/Deno/whatever)                          
  ┌─────────────────────────────────────────────────────────┐    
  │                                                         │    
  │   Application code                                      │    
  │      │                                                  │    
  │      ▼                                                  │    
  │   @iso4/host (this package, TypeScript)                 │    
  │      ├─ spawns the Rust V8 binary once per process       │   
  │      ├─ multiplexes runs onto isolates via UDS           │   
  │      ├─ owns the imports/globals registry per-run        │   
  │      └─ enforces export size + serialization rules       │   
  │                                                         │    
  └────────────────────────────┬────────────────────────────┘    
                               │  Unix domain socket               
                               │  length-prefixed binary frames    
                               ▼                                   
  ┌─────────────────────────────────────────────────────────┐    
  │ iso4-v8 (Rust binary, ships per-platform via npm)         │  
  │                                                         │    
  │   main.rs  ─ UDS accept loop, auth, signal handling      │   
  │      │                                                  │    
  │      ├─ one OS thread per isolate                       │    
  │      │     v8::Isolate (heap_limits + custom allocator) │    
  │      │     v8::Context with curated globals             │    
  │      │     timeout guard thread (terminate_execution)   │    
  │      │     CPU budget tracker (enter/leave bracketing)  │    
  │      └─ V8 startup snapshot for sub-ms isolate boot     │    
  └─────────────────────────────────────────────────────────┘    
```

The host TS package does _not_ call into V8 directly. It only sends frames.
The Rust binary owns all V8 state.

### 2.1 Why out-of-process

`v8::V8::initialize_platform()` is process-global. Node already initialized
its own V8 platform with its own version, so we cannot link `rusty_v8` into
the host process. Out-of-process is the only architecture that:

- Lets us pick our own V8 version independent of the host.
- Gives crash isolation: a V8 bug, OOM, or stack overflow takes down the
  Rust process, not the host.
- Works under any host runtime (Node, Bun, Deno, browsers via WS bridge).

The IPC overhead is ~30–100 μs per bridge call. That's invisible compared to
a `fetch` (which is the only async thing users will call frequently). For
pure compute that never crosses the boundary, IPC cost is zero.

### 2.2 Why TypeScript host, not Rust-via-NAPI

Considered. Rejected. The cost between host and isolate is dominated by the
IPC syscall + V8 serialization, not the host language. Rewriting the host in
Rust via NAPI adds a NAPI boundary cost on top of the unchanged IPC cost,
makes per-platform builds harder, and doesn't enable shared memory (separate
address spaces). If profiling later proves the host is a bottleneck, the
public API is engine-agnostic enough to swap implementations behind it.

---

## 3. The execution model

Every call to `runtime.run(opts)`:

1. Host opens a fresh isolate session in the Rust process.
2. Rust spawns a thread, acquires a concurrency slot, creates a
   `v8::Isolate` with the configured heap limit and custom array-buffer
   allocator.
3. Rust creates a fresh `v8::Context`. Installs only deliberate runtime-owned
   globals. Log capture (`console.*`) is a separate design item and must not be
   smuggled in as an ad-hoc inline prelude. Host-configured globals/functions
   are installed through the bridge surface declared by the host.
4. Rust compiles `opts.code` as a `v8::Module` (always ESM) using a module
   resolver that:
   - Looks up each `import` specifier in the host's `imports` map.
   - For source modules: compiles and caches the `v8::Module`.
   - For host-implemented modules: builds a synthetic module whose
     exports are stubs that bridge to the host.
5. Rust evaluates the module. Top-level `await` works.
6. Once evaluation settles, Rust reads the module namespace, runs V8
   `ValueSerializer` on `default` and each named export. Function values
   throw `ExportNotSerializable`. Methods on class instances are stripped
   silently (serializer copies own enumerable properties only).
7. Rust sends back: serialized exports, captured stdout/stderr, error if
   any, duration.

### 3.1 What runs where

| Concern                             | Where It Lives                                        |
| ----------------------------------- | ----------------------------------------------------- |
| User JS code                        | V8 isolate in Rust process                            |
| Source modules (Flavor B)           | Compiled into V8, runs in-isolate                     |
| Host-implemented modules (Flavor A) | Stubs in V8 call across bridge → host                 |
| `fetch`                             | Stub in V8, host implements with permission check     |
| `console.*`                         | Captured in Rust, streamed back as stdout/stderr      |
| Result serialization                | V8 `ValueSerializer` (Rust side), constrained to data |

---

## 4. The three knobs

### 4.1 Limits

```ts
{
  memoryMb: 64,         // V8 heap + ArrayBuffer budget combined
  cpuTimeMs: 100,       // Active execution only (await-free time)
  wallTimeMs: 30_000,   // Hard backstop including async waits
  maxExportBytes: 16 * 1024 * 1024,
  maxStdoutBytes: 1 * 1024 * 1024,
  maxStderrBytes: 1 * 1024 * 1024,
}
```

**Memory** is enforced by:

- `v8::CreateParams::heap_limits(0, memoryMb * MB)` for the V8 heap.
- A custom `v8::Allocator` tracking external `ArrayBuffer` bytes against the
  same budget. Without this, `new ArrayBuffer(2**30)` bypasses heap_limits.
- `add_near_heap_limit_callback` converts OOM into a clean
  `terminate_execution` instead of a Rust-process abort.

**CPU time** is wall-clock measured but bracketed: a timer starts every time
V8 enters JS execution (`script.run`, `module.evaluate`, microtask
checkpoint, callback dispatch) and pauses every time control returns to the
Rust event loop waiting for a host response. So `await fetch(...)` does not
count against the budget; tight loops do. The cap is enforced by a thread
that calls `isolate.terminate_execution()` when the bracketed time exceeds
`cpuTimeMs`.

**Wall time** is a single guard timer that fires regardless. Catches
runaway-await cases (e.g., host fetch implementation never resolves).

### 4.2 Globals (restricted)

Globals are _not_ a free-for-all. The runtime owns most of the global
namespace; the host can only contribute from a known allowlist. Reasons:

- `console` is owned by the runtime so it can route output to
  `result.stdout` / `result.stderr` reliably.
- Globals like `URL`, `TextEncoder`, `crypto.subtle` come from V8 and must
  not be shadowed.
- Letting the host inject arbitrary names into the global namespace makes
  user-code intent ambiguous (was `myThing` user-defined or host-supplied?).

For v1, the host-providable globals are:

| Name    | Purpose                                         |
| ------- | ----------------------------------------------- |
| `fetch` | The single I/O entry point. Permission-checked. |

Everything else the host wants to expose goes through `imports`. This keeps
the global namespace small and predictable, and forces extension to happen
through a named, statically-greppable mechanism.

(If a future need arises for a second global, it goes through the same
allowlist mechanism with an explicit decision in this doc, not as a generic
"host can inject anything" knob.)

### 4.3 Imports

The full extension point. Each specifier resolves to one of:

- **Source module** — host provides JS source as a string. Compiled by V8
  once, evaluated in-isolate, cached for the lifetime of the runtime. Runs
  with zero per-call bridge cost.
- **Host module** — host provides an object of functions. The sandbox sees
  an ESM module whose exports are stubs that bridge each call back to the
  host. Slower (bridge roundtrip per call) but fully dynamic.

```ts
imports: {
  static: {
    "lodash-es": { kind: "source", source: lodashEsmBundle },
    "secrets":   { kind: "host",   exports: { get: (k) => mySecrets[k] } },
  },
  // optional dynamic resolver, runs if static map misses
  async resolve(specifier, importer) {
    if (specifier.startsWith("npm:")) {
      return { kind: "source", source: await fetchNpmEsm(specifier) };
    }
    return null;
  },
}
```

Resolution order: static map → resolver → throw `ModuleNotFound`.

Host modules can only export **functions that take serializable data and
return serializable data**. They cannot export:

- Class instances with prototype methods (methods stripped on cross).
- Functions that themselves take function arguments (no callback support
  in v1).
- Stateful object handles (`createReadStream` returning a stream).

If the user code calls a host module function with a function argument, the
bridge rejects the call with `FunctionArgumentNotSupported`. This is a
deliberate v1 limitation.

---

## 5. Result extraction

User code is always parsed as ESM. Results come back via `export`:

```js
// User code
const res = await fetch('https://api.example.com/data')
const data = await res.json()
export default data
export const fetchedAt = Date.now()
```

```ts
// Host result
{
  ok: true,
  exports: {
    default: { /* ...data... */ },
    named: { fetchedAt: 1700000000000 },
  },
  stdout: "",
  stderr: "",
  durationMs: 142,
}
```

### 5.1 What can be exported

Whatever V8 `ValueSerializer` can serialize:

- Primitives, strings, BigInt
- Arrays (including holey)
- Plain objects (own enumerable properties only — class methods stripped)
- `Date`, `RegExp`, `Map`, `Set`
- `Error` objects (name, message, stack preserved)
- Typed arrays (`Uint8Array`, etc.) and raw `ArrayBuffer`
- Circular references

What cannot be exported (throws `ExportNotSerializable`):

- Functions
- Promises (must be `await`ed before exporting)
- Symbols other than well-known ones
- Sandbox-internal handles (host-module proxy stubs, for example)

The serializer rejects functions automatically — no explicit check needed.
Methods on class instances are silently dropped because they live on the
prototype, not as own properties. This is acceptable: the goal is "only data
crosses", and dropping methods is the same behavior as `structuredClone`.

### 5.2 Errors

If user code throws (uncaught), the result is:

```ts
{
  ok: false,
  error: {
    name: "TypeError",
    message: "Cannot read property 'x' of undefined",
    code: "ERR_USER_CODE",
    stack: "...",
  },
  stdout: "...",  // whatever was emitted before the throw
  stderr: "...",
  durationMs: 42,
}
```

If the runtime kills the isolate (memory, CPU, wall):

```ts
{
  ok: false,
  error: { code: "ERR_MEMORY_LIMIT" | "ERR_CPU_TIMEOUT" | "ERR_WALL_TIMEOUT", ... },
  ...
}
```

The result is _always_ an object; `ok: true | false` discriminates. `run()`
does not throw for sandboxed failures — only for infrastructure failures
(e.g., the Rust process crashed).

---

## 6. The wire and protocol

Length-prefixed binary frames over a Unix domain socket. The canonical
reference is `docs/protocol.md`; this section is the design summary.

### 6.1 Frame format

```
┌─────────────────────┬──────────────────┬─────────────────────────┐
│  length  (4 bytes)  │  type  (1 byte)  │  payload  (N bytes)     │
│  uint32 big-endian  │  see tables      │  message-specific       │
└─────────────────────┴──────────────────┴─────────────────────────┘
```

`length` covers `type + payload` — so `length = 1` means type byte only,
no payload.

### 6.2 Message types

Direction is always known from context (each side knows whether it is
reading TS-sent or Rust-sent frames), so type bytes are scoped per
direction. Both tables start at `0x01`.

**TS → Rust**

| Byte   | Name             | Purpose                                            |
| ------ | ---------------- | -------------------------------------------------- |
| `0x01` | `Authenticate`   | First message on connect: protocol version + token |
| `0x02` | `Run`            | Start a sandboxed execution                        |
| `0x03` | `BridgeResponse` | Reply to a `BridgeCall` from Rust                  |
| `0x04` | `Terminate`      | Force-stop a running isolate                       |

**Rust → TS**

| Byte   | Name         | Purpose                                             |
| ------ | ------------ | --------------------------------------------------- |
| `0x01` | `BridgeCall` | Sandbox called `fetch` or a host-module function    |
| `0x02` | `StdioChunk` | Eager `console.*` output (stdout or stderr)         |
| `0x03` | `Result`     | Final result for a `Run` (always sent exactly once) |
| `0x04` | `Log`        | Internal runtime diagnostics                        |

### 6.3 Payload encoding

Structured data (arguments, results, exports) uses V8 `ValueSerializer`
wire format. Raw bytes for stdio chunks. Plain UTF-8 for log strings.

### 6.4 Session lifecycle and concurrency

The `Runtime` maintains a **pool of connections** to the Rust process, one
per concurrency slot. Each connection handles exactly one `Run` at a time;
concurrency comes from having multiple connections, not from multiplexing
messages on one connection.

```
Runtime (TypeScript)
  connection-pool[0]  ──UDS──▶  Rust process
  connection-pool[1]  ──UDS──▶  (same process, different isolate threads)
  connection-pool[2]  ──UDS──▶
  ...up to maxIsolates
```

When `prefix.run()` or `runtime.run()` is called:

1. Claim a free slot from the pool (or queue if all slots are busy).
2. Send `Run` on that slot's connection.
3. Receive `StdioChunk` / `BridgeCall` / `BridgeResponse` frames until
   `Result` arrives.
4. Resolve the caller's Promise and release the slot back to the pool.

This means five agents calling `prefix.run()` simultaneously each get their
own connection slot and run truly in parallel on separate isolate threads
inside the Rust process — the fifth call does not wait on the first.

`maxIsolates` in `RuntimeOptions` controls the pool ceiling.
Additional callers queue behind it (backpressure). The Rust process
spawns one OS thread per active isolate and enforces the same ceiling
server-side; connection N+1 from an overloaded client is accepted but
queued until a thread is free.

`BridgeCall` / `BridgeResponse` pairs are sequential **within a single
run** in v1: Rust sends one `BridgeCall`, waits for `BridgeResponse`,
then continues. No intra-run multiplexing. This is fine because the
bridge is fast (~30–100µs) and in-isolate JS is blocked during the
wait anyway.

---

## 7. Non-goals and known limitations

Documented up front so we don't drift into rebuilding secure-exec:

1. **No `node:*` builtins.** If a user does `import fs from "node:fs"`, it
   throws `ModuleNotFound` unless the host explicitly provided it via
   `imports`. The host can ship a curated `node:fs` if they want; the
   runtime won't.

2. **No callbacks across the boundary.** No `setTimeout`, no event
   listeners on host objects, no `array.forEach` style host callbacks. Pure
   data in, pure data out. (Future tier-2 may add a callback handle table.
   Not v1.)

3. **No streaming.** `fetch` buffers the full response body. Cap defaults to
   16 MB. Streaming would require handle-based `ReadableStream`s; not v1.

4. **No `eval` / `new Function`.** `context.allow_code_generation_from_strings(false)`
   by default. The sandbox cannot generate code from strings at runtime.
   This blocks some libraries (JSON5 parsers using `Function("return …")`).
   Override per-run if needed.

5. **No WebAssembly.** `set_allow_wasm_code_generation_callback(_ => false)`.

6. **No shared state between runs.** Each `run()` is a fresh `v8::Context`.
   Module compilation is cached by the runtime for perf; module _state_
   (singletons, top-level variables) is not.

7. **No filesystem.** No FS module is provided. If host code wants to expose
   read-only files, it does so via a custom import.

8. **No timers.** `setTimeout` and `setInterval` are not provided as globals
   in v1. (They require callback support. See limitation 2.) Tier 2.

9. **Functions on host-import return values are stripped.** If
   `imports["foo"].bar()` returns `{ data: 1, fn: () => {} }`, the sandbox
   sees `{ data: 1 }`. Same rule as exports.

10. **Identity is not preserved across the boundary.** Not relevant in v1
    because nothing crosses by reference; flagged here for future tiers.

---

## 8. Project layout

Monorepo with pnpm workspaces. The canonical breakdown lives in
`MONOREPO.md`; the short version:

```
iso4/
  DESIGN.md                         ← this file
  MONOREPO.md                       ← package boundary rules
  packages/iso4-dynamic/src/types.ts ← canonical API shapes for @iso4/dynamic
  packages/iso4-core/src/types.ts    ← shared types (@iso4/core)
  pnpm-workspace.yaml
  packages/
    iso4/                           ← the runtime: createRuntime, run, precompile
    iso4-fetch/                     ← @iso4/fetch — hardened fetch helpers
    iso4-fs/                        ← @iso4/fs    — (future) node:fs stub factory
    iso4-crypto/                    ← @iso4/crypto (future) node:crypto stub factory
    iso4-v8-darwin-arm64/           ← @iso4/v8-darwin-arm64 — native Rust binary
    iso4-v8-linux-x64-gnu/          ← (additional platforms added as needed)
    ...
  native/
    v8-runtime/                     ← Rust source for the V8 host binary
```

Target sizes (when complete):

- `iso4`: <1500 LoC TypeScript.
- `@iso4/fetch`: <500 LoC TypeScript.
- `@iso4/dynamic/v8-runtime`: <3000 LoC Rust.
- JS injected into each isolate: <300 LoC.

---

## 9. Phased build plan

Reordered so the snapshot-based prefix mechanism lands early — it's the
feature most users will rely on and shapes the IPC protocol.

| Phase | Scope                                                                                                                                     | Deliverable                                                                |
| ----- | ----------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------- |
| 0     | This doc + `packages/iso4-dynamic/src/types.ts` + `packages/iso4-core/src/types.ts` + `MONOREPO.md` + workspace scaffolding               | API committed before code                                                  |
| 1     | Rust binary: spawn, UDS, single run, heap limit, CPU timeout (wall-clock), ESM compile + evaluate, captured console, export serialization | `runtime.run({ code, limits })` works end-to-end with no imports, no fetch |
| 2     | Precompile + `PrecompiledPrefix.run()` via V8 startup snapshots                                                                           | The canonical AI-agent prefix/postfix loop works                           |
| 3     | CPU budget enter/leave bracketing (async time exclusion)                                                                                  | Tight loops killed quickly; `await fetch` doesn't burn budget              |
| 4     | `fetch` global with bridge-side header/URL validation; permission predicate; body cap; abort-on-timeout                                   | Real network usable with deny-by-default control                           |
| 5     | `@iso4/fetch` package: `createSafeFetch` with allowlist, DNS pin, private-IP blocking, no-auto-redirect                                   | Hardened default users can opt into in two lines                           |
| 6     | Imports: source modules (Flavor B) with V8 code cache                                                                                     | `import * as z from "zod"` works when host provides source                 |
| 7     | Imports: host modules (Flavor A) with synthetic V8 modules                                                                                | `import fs from "node:fs"` works when host provides functions              |
| 8     | Custom `ArrayBuffer` allocator, near-heap-limit graceful kill, hard wall-clock guard separate from CPU budget                             | Memory and time limits are tight under adversarial input                   |
| 9     | Pre-warmed isolate pool (optional, behind a runtime option)                                                                               | Sub-2ms cold start for high-throughput workloads                           |
| 10    | Polish: error types, integration tests, READMEs, examples                                                                                 | Shippable v1                                                               |
| 11    | `prefix.openSession()` + `Session` API; `HostCall`/`HostCallResult` wire messages; persistent-isolate semantics                           | Analytics pipeline use case works end-to-end with the two-process backend  |
| 12    | In-process (C++ NAPI) backend behind a `RuntimeOptions.backend` flag; same `Session` API; requires Docker/K8s outer isolation             | Sub-µs amortized per-call overhead for high-throughput analytics           |

Each phase is independently shippable. We stop and reassess at the end of
each phase.

Phases 11–12 are post-v1. They are documented here because they constrain
the v1 API shape: `prefix.openSession()` must be addable without breaking
`prefix.run()`, and `RuntimeOptions` must accommodate a `backend` field
without restructuring. No code changes needed now; just don't close those
doors.

---

## 10. Open questions

To be resolved as we build, not blocking the start:

- **Single-isolate-per-process or multi-isolate?** Resolved: **multi-isolate
  from the start**. The Runtime manages a connection pool; each slot has its
  own UDS connection and its own isolate thread in the Rust process. Pool
  size = `maxIsolates` (default: CPU count). This is required for MCP
  multi-agent parallelism and for the analytics session model.

- **How aggressive should the export validator be?** Strict-throw on
  functions is a hard requirement. Question: do we also throw on
  unresolved Promises, or just await them once at the top level? Current
  lean: throw, force the user to await before exporting.

- **How exactly should logs/stdout/stderr be captured?**
  Log handling is deliberately deferred. Options include native V8 callbacks,
  a small owned shim installed by the runtime, eager `StdioChunk` frames, or
  buffered output returned with `Result`. Do not add an ad-hoc inline JS
  prelude while this is unresolved. Current lean: eager, frame-per-write,
  with a 1 MB cap per stream, but this still needs an explicit implementation
  decision.

- **Snapshot cache LRU cap.** Snapshots live in the Rust process's memory.
  Each is tens-to-hundreds of KB. Default cap: 100 snapshots, LRU evicted.
  Configurable via `RuntimeOptions.maxPrecompiledPrefixes`. Evicted handles
  fail `.run()` with `ERR_PREFIX_DISPOSED`; the host re-precompiles.

- **Mandate `createSafeFetch` or recommend it?** The core `iso4` package's
  `fetch` field accepts any `FetchHandler`. The library does mechanical
  hygiene (header/URL validation) at the bridge; the host author decides
  policy (allow/deny). `@iso4/fetch` ships hardened defaults but is opt-in.
  Documented strongly.

- **`Session.call()` input/output serialization contract.** For the
  persistent-session API (Phase 11), the host calls a function that was
  exported by the prefix. What can be passed as `input`? V8 `ValueSerializer`
  is the natural choice (same as exports today), but typed arrays could be
  transferred zero-copy if needed for bulk data. Decide when designing Phase 11.

- **In-process backend: C++ NAPI addon or Rust NAPI with Node's V8
  headers?** `rusty_v8` cannot be used in-process because it calls
  `v8::V8::initialize_platform()`, which Node already did. A C++ NAPI
  addon (like `isolated-vm`) is the proven path. Rust via raw `bindgen`
  to Node's V8 headers is possible but undocumented territory. Decide
  when Phase 12 is prioritised.

## 11. Performance and the prefix/postfix pattern

### 11.1 Why this matters

The canonical AI-agent loop is:

```
host  : prepare context + tools     ← fixed across many runs
agent : generate code that uses them
host  : execute (host prefix + agent postfix), get result
```

If every iteration of this loop pays a full V8 cold start (compile bridge
shims, set up globals, parse user code), the human-visible latency is
dominated by ~30–80 ms of V8 boot regardless of how short the postfix is.
For an interactive agent doing N tool calls per turn, that compounds badly.

### 11.2 What we do about it

V8 startup snapshots let us serialize a fully-initialized isolate state to
a `v8::StartupData` blob and restore it on demand. The `Runtime.precompile()`
entry point creates a snapshot of a user-supplied prefix; `prefix.run()`
restores it and runs the postfix.

Flow:

```
  precompile(prefix)            run(postfix)         run(postfix)
       │                            │                     │
       ▼                            ▼                     ▼
  one-time:                    fast path:            fast path:
  - boot isolate               - isolate from        - same
  - install globals stubs        snapshot (~1ms)
  - run prefix code            - rebind per-run
  - create_blob() → snapshot     globals & imports
  - cache snapshot             - compile postfix
                               - run
```

Steady-state cold start target (after the first call): **<5 ms** from
`prefix.run()` to user code executing.

### 11.3 What the snapshot captures vs doesn't

The snapshot captures the V8 heap state at the moment the prefix module
finishes evaluating. That means:

- **Captured (baked in, identical across runs):**
  - All top-level `const`/`let`/`var` bindings.
  - All compiled `v8::Module`s the prefix imported.
  - Closures, function objects, frozen built-in shapes.
  - The results of any I/O the prefix did (because the resolved values
    are now in memory).
- **Not captured (rebound per run):**
  - The backing C++ pointers for bridge function stubs. These point at the
    precompile-time session, which is gone after snapshotting. On restore,
    the Rust runtime walks the snapshot's global object and replaces every
    bridge stub with a session-local one bound to the new run's handlers.
    Same trick secure-exec uses.
- **Forbidden:**
  - You cannot snapshot an isolate with pending Promises, in-flight bridge
    calls, or any external resource state. The prefix module must complete
    evaluation (top-level await included) before the snapshot is taken.

### 11.4 Rebinding rules

When `prefix.run()` provides `globals` or `imports`, the Rust runtime
looks up each name in the snapshot's global object (for globals) or its
module registry (for host imports) and replaces the underlying bridge
handler pointer. Source modules cannot be rebound — their code is frozen
in the snapshot.

If `prefix.run()` passes a name that wasn't declared at `precompile()`
time, the run fails fast with `ERR_UNDECLARED_BINDING`. This is intentional:
we could silently install new globals into the restored context, but that
breaks the invariant that the prefix snapshot represents the full shape of
the sandbox surface. Better to be strict and force the user to declare
their surface up front.

If the prefix declared a global the run doesn't supply, the precompile-time
implementation is reused. This makes it easy to provide a default at
precompile time and override only when needed per run.

### 11.5 Pool of pre-warmed isolates (phase 9)

Even with snapshots, `v8::Isolate::new` + snapshot restore costs ~1–2 ms.
For high-throughput workloads we keep a pool of N already-restored isolates
per precompiled prefix, sitting idle until a run grabs one. Pool size,
eviction policy, and warmup-on-precompile are all `RuntimeOptions`.

Not in v1. Snapshots alone get steady-state cold start under 5 ms which is
plenty for interactive agents. The pool is for sub-millisecond requirements.

## 12. Security model — fetch hardening

This section codifies the responsibility split between the library and the
host author. The short version: the **library** must do mechanical hygiene
that no one would get right at the application layer; the **host author**
must decide policy. The library cannot decide policy. The host author
should not have to worry about CRLF injection.

### 12.1 What actually happens when sandbox JS calls a host function such as `fetch`

`fetch` is not special in the V8 runtime. If the host wants to expose a
`fetch`-like function, it is installed through the same host bridge mechanism
as any other configured global/function or host import. The sandbox call is
bundled as plain data, Rust sends a `BridgeCall`, the TypeScript host runs the
configured handler, and the result is passed back via `BridgeResponse`.

**The bridge does NOT auto-route to `globalThis.fetch` on the host.** If the
host author writes a handler that calls `globalThis.fetch`, then yes, the
host's real network stack runs with sandbox-controlled inputs. If they write
something restrictive, that runs. The bridge is just a function-call protocol.

### 12.2 Attack categories and where each is mitigated

Fetch/network policy and hygiene live in the configured host handler or in a
helper package such as `@iso4/fetch`, not in V8 execution itself. The runtime
should not grow fetch-specific error codes; handler failures surface through
the generic host-bridge error path.

| Attack                                                           | Where It Lives                                                                                                  | Who Mitigates                                                                                                               |
| ---------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------- |
| CRLF/NUL injection in header values                              | Sandbox sends bad bytes that, if forwarded raw, let a naive HTTP client smuggle headers or open second requests | The configured fetch handler / `@iso4/fetch`.                                                                               |
| Non-http URL schemes (`file:`, `data:`, `javascript:`)           | Sandbox sends a URL string a permissive client might try to fetch                                               | The configured fetch handler / `@iso4/fetch`.                                                                               |
| URL parse-vs-request mismatch                                    | Permission check parses URL one way, host's HTTP client parses another (`http://a@b/`)                          | The configured fetch handler / `@iso4/fetch`.                                                                               |
| Body size DoS                                                    | Sandbox sends a 1 GB body                                                                                       | The configured fetch handler / `@iso4/fetch`.                                                                               |
| Response size amplification                                      | Server returns compressed response that decompresses huge                                                       | The configured fetch handler / `@iso4/fetch`.                                                                               |
| SSRF to internal services (`169.254.169.254`, RFC1918, loopback) | Sandbox URL points at host-local services                                                                       | `@iso4/fetch` (opt-in) pre-resolves DNS, refuses private/link-local IPs unless allowlisted.                                 |
| DNS rebinding                                                    | Hostname allowlisted, attacker rebinds DNS between permission check and actual fetch                            | `@iso4/fetch` pre-resolves once at bridge layer, makes the actual request to the resolved IP with explicit `Host:` header.  |
| Redirect-based bypass                                            | Allowed host responds with 3xx to internal URL; client follows blindly                                          | The configured fetch handler / `@iso4/fetch`.                                                                               |
| Auth-header leakage via shared HTTP client                       | Host wires `fetch` to their app's authenticated axios instance; sandbox now uses host's auth tokens             | Host author — use a clean HTTP client for sandboxed traffic. `@iso4/fetch` uses an isolated `undici` Dispatcher by default. |
| Permission policy (which URLs/methods/etc. are allowed)          | Application-specific                                                                                            | Host author, expressed in the configured handler. `@iso4/fetch` provides allowlist helpers.                                 |
| Timing oracle on permission denial                               | Sandbox times responses to detect reachability of internal hosts                                                | Mostly host author. `@iso4/fetch` can normalize denial latency.                                                             |

### 12.3 Package boundaries for fetch

- **`iso4`** ships:
  - Generic host-bridge plumbing for configured globals/functions and imports.
  - No default HTTP client. `fetch` is not special-cased as an always-present
    runtime global; if the host wants to expose it, it is configured through
    the same bridge mechanism as other host-provided globals/functions.

- **`@iso4/fetch`** ships:
  - `createSafeFetch({ allowedHosts, blockPrivateIPs, maxRedirects, timeout, ... })`
    returning a hardened `FetchHandler`.
  - DNS pre-resolution with IP pinning.
  - Isolated `undici` Dispatcher (no shared connection pool with the host).
  - Per-host rate limiting hooks.

- **Host author** writes:
  ```ts
  import { createRuntime } from "@iso4/dynamic";
  import { createSafeFetch } from "@iso4/fetch";

  const runtime = await createRuntime();
  const prefix = await runtime.precompile({
    code: prefixSource,
    globals: { fetch: createSafeFetch({ allowedHosts: [...] }) },
  });
  const result = await prefix.run({ code: agentCode });
  ```

A host author who knows what they're doing can pass any `FetchHandler` they
want. A host author who isn't an HTTP-security expert should reach for
`@iso4/fetch` and not write their own. The docs lean hard on this.

---

## 13. Two execution models

This section makes the §1.1/§1.2 distinction concrete at the API and
protocol level. It is forward-looking — only Model 1 is built in v1. Model
2 is designed here so that v1 decisions don't accidentally close the door.

### 13.1 Model 1 — One-shot runs (v1)

```
prefix.run({ code }) → RunResult
```

The isolate is created from the snapshot, executes a code string, returns
named exports, and is torn down. Every run is independent. IPC cost: one
round trip (Run → ... → Result). Suitable when the work per run is
non-trivial and async (fetch, host imports).

This is the AI-agent prefix/postfix pattern described in §11.

### 13.2 Model 2 — Persistent sessions (Phase 11+)

```
const session = await prefix.openSession(options)
const result  = await session.call('transformRow', input)   // many times
await session.close()
```

The isolate is created from the snapshot once and kept alive for the
lifetime of the session. The prefix exports a function (`transformRow` above)
that the host calls repeatedly with different inputs. No new isolate cost
per call; no snapshot restore per call.

**What the prefix must export for this to work:**

```js
// prefix code
export function transformRow(row) {
  return { revenue: row.price * row.qty, margin: row.margin }
}
```

`session.call(name, input)` invokes that exported function by name with a
single input value. The return value is the call result. Input and output
both cross via V8 `ValueSerializer` (two-process backend) or structured
clone (in-process backend).

### 13.3 Session concurrency — same connection-pool model as runs

Sessions use the same connection pool as one-shot runs (§6.4). A session
borrowing a slot holds it open for its lifetime rather than returning it
after one message exchange. The **connection is the session** — no
session-id routing needed on the wire.

```
Runtime (TypeScript)
  pool-slot[0] ──UDS──▶ Rust isolate thread 0  ← session A (long-lived)
  pool-slot[1] ──UDS──▶ Rust isolate thread 1  ← session B (long-lived)
  pool-slot[2] ──UDS──▶ Rust isolate thread 2  ← one-shot run
```

Three concurrent requests (session A, session B, one run) each hold their
own slot and execute in parallel. The slot is released when the session
is closed or the run completes.

Lifecycle on a single connection:

```
TS (one pool slot)                   Rust (one isolate thread)
│                                       │
│──── Authenticate ────────────────────▶│
│──── OpenSession ─────────────────────▶│  boot isolate from snapshot
│◀─── SessionOpened ───────────────────│
│                                       │
│──── Call(fn="transformRow", row_a) ────▶│  invoke exported fn, return result
│◀─── CallResult(result_a) ─────────────│
│──── Call(fn="transformRow", row_b) ────▶│
│◀─── CallResult(result_b) ─────────────│
│                                       │
│──── CloseSession ─────────────────────▶│  teardown or return to session pool
│                                       │
│  (slot released back to Runtime pool) │
```

Calls within a session are sequential (one `Call` at a time per connection),
which is fine because the transform function is synchronous and fast.
Parallelism across requests comes from multiple sessions holding multiple
slots simultaneously — the same mechanism that makes parallel `run()` calls
work for MCP agents.

### 13.4 Session pool — pre-warmed isolates for instant open

Opening a session cold (snapshot restore + thread spawn) costs ~1–2 ms.
For a workload that receives bursts of parallel requests, that latency
stacks. The session pool solves this: the runtime pre-warms N isolates
from the prefix snapshot and holds them idle. `prefix.openSession()`
picks one off the pool instantly.

```ts
const prefix = await runtime.precompile({
  code: transformCode,
  sessionPool: { size: 10 }, // pre-warm 10 isolates immediately
})

// 5 requests arrive simultaneously — all get isolates in ~0 ms
const sessions = await Promise.all(
  requests.map(() => prefix.openSession())
)
const results = await Promise.all(
  sessions.map((s, i) => s.call('transformRow', requests[i].input))
)
await Promise.all(sessions.map((s) => s.close())) // returns isolates to pool
```

Pool semantics:

- `openSession()` on a full pool returns immediately (O(1) isolate hand-off).
- `openSession()` when the pool is empty either cold-starts a new isolate
  (default) or waits for one to become free if `sessionPool.maxSize` is
  set.
- `session.close()` returns the isolate to the pool (snapshot-restores it
  in place) rather than destroying it, so it's ready for the next request.
- Pool size, max size, and idle eviction timeout are all
  `PrecompileOptions.sessionPool` fields. Decided at Phase 11.

This is the session analogue of Phase 9's pre-warmed isolate pool for
one-shot runs.

**Wire protocol additions (two-process backend, Phase 11):**

TS→Rust:

| Byte   | Name           | Payload                           |
| ------ | -------------- | --------------------------------- |
| `0x05` | `OpenSession`  | `u32` prefix-id                   |
| `0x06` | `Call`         | UTF-8 fn name + V8-serialized arg |
| `0x07` | `CloseSession` | _(empty)_                         |

Rust→TS:

| Byte   | Name            | Payload                          |
| ------ | --------------- | -------------------------------- |
| `0x05` | `SessionOpened` | _(empty — connection is the id)_ |
| `0x06` | `CallResult`    | V8-serialized result or error    |

No session-id on the wire: the connection itself identifies the session,
just as the connection identifies which `Run` is active.

**No async host bridge in session mode.** Session calls are synchronous from
the isolate's perspective. If the transform function tries to call an
unconfigured host global/function such as `fetch`, it fails like ordinary
user code (for example, a missing binding), not with a fetch-specific runtime
error. The session API is intentionally narrow: pure data in, pure data out,
no async I/O.

### 13.3 In-process backend (Phase 12)

The in-process backend implements the same `Session` API using a C++ NAPI
addon rather than the Rust subprocess. `session.call()` becomes a direct
V8 function invocation with no UDS round trip.

**When to use it:** Analytics pipelines running inside Docker/Kubernetes
where the container is the hard security boundary, the code is written by
developers (trusted enough), and per-call throughput above ~10 000
calls/second is required.

**When not to use it:** Any deployment without OS-level isolation around the
Node process. An OOM inside the isolate can crash the Node process with the
in-process backend. The two-process backend's crash isolation does not apply.

**API compatibility:** The `Session` interface is identical regardless of
backend. The backend is selected at `createRuntime()` time via
`RuntimeOptions.backend`:

```ts
// Two-process (default, v1)
const runtime = await createRuntime()

// In-process (Phase 12, opt-in)
const runtime = await createRuntime({ backend: 'inprocess' })
```

Both produce the same `Runtime`, `PrecompiledPrefix`, and `Session` objects.
The one-shot `prefix.run()` also works with the in-process backend (it
creates a session, runs the code as the session's body, returns exports,
closes the session).

### 13.4 Choosing between the models

|                        | One-shot `run()`    | Persistent `session.call()` |
| ---------------------- | ------------------- | --------------------------- |
| Isolate lifetime       | per-call            | per-session                 |
| Async / fetch          | ✅                  | ❌ (sync only)              |
| Imports (host modules) | ✅                  | source-only                 |
| IPC cost (two-process) | once per run        | once per call               |
| IPC cost (in-process)  | ~1ms                | ~1–5 µs                     |
| Per-row analytics      | impractical         | ✅                          |
| Crash isolation        | ✅ always           | ✅ two-process only         |
| Prompt-injection risk  | mitigated by limits | same                        |
