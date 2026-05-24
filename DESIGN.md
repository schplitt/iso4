# iso4 — Design

A small, sharp V8-isolate sandbox for running untrusted JavaScript with
controllable memory and execution-time limits, host-curated network access,
and a clean module-resolution API. Successor to the secure-exec idea, but with
none of the Node-compatibility surface area.

---

## 1. Goals

In scope:

1. **Pure JavaScript execution** in V8 isolates, one isolate per run.
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
7. **Result extraction via ESM `export`**. User code is ESM, can `export default`
   and named exports. Results cross the boundary through V8 `ValueSerializer`,
   which naturally restricts results to plain data (no functions, no methods).
8. **Captured stdout/stderr** via a runtime-owned `console`.
9. **Pre-compilable prefix code** via V8 startup snapshots. The canonical
   AI-agent pattern (host setup + agent-generated postfix) hits ~2–5 ms
   steady-state cold start by snapshotting the prefix once and reusing it.
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
  booted from the prefix snapshot — the *prefix's heap shape* is shared,
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

The host TS package does *not* call into V8 directly. It only sends frames.
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
3. Rust creates a fresh `v8::Context`. Installs runtime-owned globals
   (`console`, `crypto.getRandomValues`, etc. — V8 built-ins or tiny shims).
   Installs host-allowed globals from `opts.globals` (currently just `fetch`).
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

| Concern | Where it lives |
|---|---|
| User JS code | V8 isolate in Rust process |
| Source modules (Flavor B) | Compiled into V8, runs in-isolate |
| Host-implemented modules (Flavor A) | Stubs in V8 call across bridge → host |
| `fetch` | Stub in V8, host implements with permission check |
| `console.*` | Captured in Rust, streamed back as stdout/stderr |
| Result serialization | V8 `ValueSerializer` (Rust side), constrained to data |

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

Globals are *not* a free-for-all. The runtime owns most of the global
namespace; the host can only contribute from a known allowlist. Reasons:

- `console` is owned by the runtime so it can route output to
  `result.stdout` / `result.stderr` reliably.
- Globals like `URL`, `TextEncoder`, `crypto.subtle` come from V8 and must
  not be shadowed.
- Letting the host inject arbitrary names into the global namespace makes
  user-code intent ambiguous (was `myThing` user-defined or host-supplied?).

For v1, the host-providable globals are:

| Name | Purpose |
|---|---|
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
const res = await fetch("https://api.example.com/data");
const data = await res.json();
export default data;
export const fetchedAt = Date.now();
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

The result is *always* an object; `ok: true | false` discriminates. `run()`
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

| Byte   | Name             | Purpose                                      |
|--------|------------------|----------------------------------------------|
| `0x01` | `Authenticate`   | First message on connect: protocol version + token |
| `0x02` | `Run`            | Start a sandboxed execution                  |
| `0x03` | `BridgeResponse` | Reply to a `BridgeCall` from Rust            |
| `0x04` | `Terminate`      | Force-stop a running isolate                 |

**Rust → TS**

| Byte   | Name          | Purpose                                               |
|--------|---------------|-------------------------------------------------------|
| `0x01` | `BridgeCall`  | Sandbox called `fetch` or a host-module function      |
| `0x02` | `StdioChunk`  | Eager `console.*` output (stdout or stderr)           |
| `0x03` | `Result`      | Final result for a `Run` (always sent exactly once)   |
| `0x04` | `Log`         | Internal runtime diagnostics                          |

### 6.3 Payload encoding

Structured data (arguments, results, exports) uses V8 `ValueSerializer`
wire format. Raw bytes for stdio chunks. Plain UTF-8 for log strings.

### 6.4 Session lifecycle

One connection per runtime instance. Multiple `Run` messages may be sent
sequentially on one connection (after each `Result` is received). Concurrent
runs on one connection are not supported in v1 — each `Run` must complete
before the next begins.

`BridgeCall` / `BridgeResponse` pairs are also sequential within a run in
v1: Rust sends one `BridgeCall`, waits for `BridgeResponse`, then continues.
No multiplexing.

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
   Module compilation is cached by the runtime for perf; module *state*
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
  packages/iso4/src/types.ts        ← canonical API shapes
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
- `native/v8-runtime`: <3000 LoC Rust.
- JS injected into each isolate: <300 LoC.

---

## 9. Phased build plan

Reordered so the snapshot-based prefix mechanism lands early — it's the
feature most users will rely on and shapes the IPC protocol.

| Phase | Scope | Deliverable |
|-------|-------|-------------|
| 0 | This doc + `packages/iso4/src/types.ts` + `MONOREPO.md` + workspace scaffolding | API committed before code |
| 1 | Rust binary: spawn, UDS, single run, heap limit, CPU timeout (wall-clock), ESM compile + evaluate, captured console, export serialization | `runtime.run({ code, limits })` works end-to-end with no imports, no fetch |
| 2 | Precompile + `PrecompiledPrefix.run()` via V8 startup snapshots | The canonical AI-agent prefix/postfix loop works |
| 3 | CPU budget enter/leave bracketing (async time exclusion) | Tight loops killed quickly; `await fetch` doesn't burn budget |
| 4 | `fetch` global with bridge-side header/URL validation; permission predicate; body cap; abort-on-timeout | Real network usable with deny-by-default control |
| 5 | `@iso4/fetch` package: `createSafeFetch` with allowlist, DNS pin, private-IP blocking, no-auto-redirect | Hardened default users can opt into in two lines |
| 6 | Imports: source modules (Flavor B) with V8 code cache | `import * as z from "zod"` works when host provides source |
| 7 | Imports: host modules (Flavor A) with synthetic V8 modules | `import fs from "node:fs"` works when host provides functions |
| 8 | Custom `ArrayBuffer` allocator, near-heap-limit graceful kill, hard wall-clock guard separate from CPU budget | Memory and time limits are tight under adversarial input |
| 9 | Pre-warmed isolate pool (optional, behind a runtime option) | Sub-2ms cold start for high-throughput workloads |
| 10 | Polish: error types, integration tests, READMEs, examples | Shippable v1 |

Each phase is independently shippable. We stop and reassess at the end of
each phase.

---

## 10. Open questions

To be resolved as we build, not blocking the start:

- **Single-isolate-per-process or multi-isolate?** Secure-exec multiplexes
  many isolates onto one Rust process. Simpler v1: one isolate per Rust
  process, spawn one per `run()`. Worse latency, vastly simpler.
  Default decision: **multi-isolate from the start**, with a configurable
  concurrency cap, because cold-spawn of the Rust binary including V8
  platform init is 50–100ms and we'd lose that on every run.

- **How aggressive should the export validator be?** Strict-throw on
  functions is a hard requirement. Question: do we also throw on
  unresolved Promises, or just await them once at the top level? Current
  lean: throw, force the user to await before exporting.

- **Should `console.log` flush eagerly to host or buffer until run ends?**
  Eager flushing lets the host show progress for long-running scripts.
  Buffering is simpler. Lean: eager, frame-per-write, with a 1 MB cap per
  stream.

- **Snapshot cache LRU cap.** Snapshots live in the Rust process's memory.
  Each is tens-to-hundreds of KB. Default cap: 100 snapshots, LRU evicted.
  Configurable via `RuntimeOptions.maxPrecompiledPrefixes`. Evicted handles
  fail `.run()` with `ERR_PREFIX_DISPOSED`; the host re-precompiles.

- **Mandate `createSafeFetch` or recommend it?** The core `iso4` package's
  `fetch` field accepts any `FetchHandler`. The library does mechanical
  hygiene (header/URL validation) at the bridge; the host author decides
  policy (allow/deny). `@iso4/fetch` ships hardened defaults but is opt-in.
  Documented strongly.

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

### 12.1 What actually happens when sandbox JS calls `fetch`

The sandbox `fetch` global is a tiny stub that bundles the request as a
plain data object and bridges to the host. The host's configured
`FetchHandler` runs. Whatever the handler does is the host author's choice.

**The bridge does NOT auto-route to `globalThis.fetch` on the host.** If
the host author writes `fetch: (req) => globalThis.fetch(req.url, req)`,
then yes, the host's real network stack runs with sandbox-controlled
inputs. If they write something restrictive, that runs. The bridge is
just a function-call protocol.

### 12.2 Attack categories and where each is mitigated

| Attack | Where it lives | Who mitigates |
|---|---|---|
| CRLF/NUL injection in header values | Sandbox sends bad bytes that, if forwarded raw, let a naive HTTP client smuggle headers or open second requests | **`@iso4/core`** validates header names and values at the bridge boundary before invoking the host handler. Rejects with `ERR_FETCH_INVALID_HEADER`. |
| Non-http URL schemes (`file:`, `data:`, `javascript:`) | Sandbox sends a URL string a permissive client might try to fetch | **`@iso4/core`** parses URL via WHATWG URL, rejects non-http(s) with `ERR_FETCH_INVALID_URL`. |
| URL parse-vs-request mismatch | Permission check parses URL one way, host's HTTP client parses another (`http://a@b/`) | **`@iso4/core`** canonicalizes via WHATWG URL once; same canonical string passed to handler. |
| Body size DoS | Sandbox sends a 1 GB body | **`@iso4/core`** enforces `limits.maxFetchBodyBytes` before crossing to host. |
| Response size amplification | Server returns compressed response that decompresses huge | **`@iso4/core`** caps response bytes pre-decompression at the bridge. Hardened handlers (`@iso4/fetch`) refuse `Content-Encoding` unless explicitly enabled. |
| SSRF to internal services (`169.254.169.254`, RFC1918, loopback) | Sandbox URL points at host-local services | **`@iso4/fetch`** (opt-in) pre-resolves DNS, refuses private/link-local IPs unless allowlisted. |
| DNS rebinding | Hostname allowlisted, attacker rebinds DNS between permission check and actual fetch | **`@iso4/fetch`** pre-resolves once at bridge layer, makes the actual request to the resolved IP with explicit `Host:` header. |
| Redirect-based bypass | Allowed host responds with 3xx to internal URL; client follows blindly | **`@iso4/core`** disables auto-redirect in the bridge. 3xx surfaces to sandbox as a normal response; sandbox code must call `fetch` again to follow. Permission check runs on each call. |
| Auth-header leakage via shared HTTP client | Host wires `fetch` to their app's authenticated axios instance; sandbox now uses host's auth tokens | **Host author** — use a clean HTTP client for sandboxed traffic. `@iso4/fetch` uses an isolated `undici` Dispatcher by default. |
| Permission policy (which URLs/methods/etc. are allowed) | Application-specific | **Host author**, expressed as a predicate inside the configured handler. `@iso4/fetch` provides allowlist helpers. |
| Timing oracle on permission denial | Sandbox times responses to detect reachability of internal hosts | Mostly **host author**. `@iso4/fetch` can normalize denial latency. |

### 12.3 Package boundaries for fetch

- **`iso4`** ships:
  - Bridge-side validation of header names/values, URL scheme, method, body
    size, response size cap.
  - No-auto-redirect by default.
  - The `FetchHandler` interface every host implementation must conform to.
  - No default HTTP client — a configured `fetch` is required for sandbox
    code to actually make network calls, otherwise `ERR_FETCH_NOT_CONFIGURED`.

- **`@iso4/fetch`** ships:
  - `createSafeFetch({ allowedHosts, blockPrivateIPs, maxRedirects, timeout, ... })`
    returning a hardened `FetchHandler`.
  - DNS pre-resolution with IP pinning.
  - Isolated `undici` Dispatcher (no shared connection pool with the host).
  - Per-host rate limiting hooks.

- **Host author** writes:
  ```ts
  import { createRuntime } from "iso4";
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
