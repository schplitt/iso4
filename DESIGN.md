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

The static-code shape above is achievable in `@iso4/sandbox` via
`prefix.run()` with a per-run host import that provides the input data —
at the cost of a snapshot restore per call. A persistent-session API
(`session.call('transform', row)`) that reuses the isolate across calls
is **not** part of `@iso4/sandbox`; it belongs to the future analytics
product (see §9.1 and §13.1).

Both shapes use the same `PrecompiledPrefix`. The prefix author declares
tools, globals, and libraries once. Dynamic vs static is a decision at
call time, not at precompile time.

### 1.2 Isolation level

`@iso4/sandbox` runs the V8 isolate in a **separate Rust process**
communicating over UDS. A crash or OOM in the isolate kills the
subprocess; the host process and all other concurrent runs are
unaffected. Adds ~30–100 µs per bridge call. Right for any deployment
where the code is not fully trusted or where crash isolation matters.

An in-process embedding (V8 inside the host process via NAPI / napi-rs)
for sub-µs call overhead is the analytics product's concern, not this
product's. See §9.1.

There is no `SandboxOptions.backend` flag and no plan to add one. The
analytics use case is served by a separate package, not a backend swap.

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
    plug into the core.

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
  memoryMb: 64,                   // V8 heap + ArrayBuffer budget combined
  cpuTimeMs: 100,                 // Active execution only (await-free time)
  wallTimeMs: 30_000,             // Hard backstop including async waits
  maxExportBytes: 16 * 1024 * 1024,
  maxStdoutBytes: 1 * 1024 * 1024,
  maxStderrBytes: 1 * 1024 * 1024,
  maxBridgeCallBytes: 16 * 1024 * 1024,     // sandbox → host args per call
  maxBridgeResponseBytes: 16 * 1024 * 1024, // host → sandbox return per call; must be ≤ memoryMb
  maxExportBytes: 16 * 1024 * 1024,         // serialised exports total
  maxStdoutBytes: 1 * 1024 * 1024,          // console.log lines; over-limit lines silently dropped
  maxStderrBytes: 1 * 1024 * 1024,          // console.warn/error lines; same truncation rule
  maxBridgeCalls: 10,                       // 0 = unlimited; default 10 protects against runaway loops
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

**Bridge payload sizes** (`maxBridgeCallBytes`, `maxBridgeResponseBytes`):
the two directions are capped independently. `maxBridgeCallBytes` limits
what untrusted sandbox code can send to host handlers; `maxBridgeResponseBytes`
limits what the host can return per call. Both violations surface as
`ERR_BRIDGE_PAYLOAD_TOO_LARGE`. `maxBridgeResponseBytes` must be ≤
`memoryMb × 1 MiB` (a response larger than the sandbox's memory budget cannot
be used); the TS layer validates this at `run()`/`precompile()` time. The Rust
poll loop additionally reads `BridgeResponse` frames with a cap of
`min(maxBridgeResponseBytes, memoryMb × 1 MiB)`, ensuring the memory budget
acts as a natural inbound frame limit.

**Export size** (`maxExportBytes`): after all exports are serialised to a
`WireValue`, Rust measures the encoded byte length. If it exceeds
`maxExportBytes` the run fails with `ERR_EXPORT_TOO_LARGE` before the
`Result` frame is written.

**Stdout/stderr sizes** (`maxStdoutBytes`, `maxStderrBytes`): Rust tracks
running byte totals during console capture. Any line whose addition would
push the total over the configured cap is silently dropped; the run continues
normally. Zero disables the cap.

### 4.2 Globals (block-listed, not allowlisted)

Globals are _not_ a free-for-all. The runtime owns a fixed set of reserved
names that the host must not shadow:

- `console` — owned by the runtime for output capture.
- V8 built-ins: `URL`, `URLSearchParams`, `TextEncoder`, `TextDecoder`,
  `crypto`, `Event`, `AbortController`, `AbortSignal`, etc.

Any name **not** on that reserved list may be provided by the host as a
global. The host passes `globals: { fetch: fn, myTool: fn, transform: fn }`
and each name becomes a bridge stub in the sandbox's global object —
callable from user code just like a built-in.

`fetch` is not special in this mechanism. It is simply the most common
global name hosts will provide. It goes through the exact same bridge path
as `myTool` or any other host-provided global (see §12.1).

Everything else the host wants to expose goes through `imports`. Globals are
for things user code expects to find as a bare name (`fetch(url)` not
`import { fetch } from 'host:net'`). When in doubt, prefer `imports` —
they are statically greppable and cannot accidentally shadow a built-in.

### 4.3 Imports

The full extension point. `imports` is a flat map keyed by specifier; the
value type is the discriminator:

- **String value — source module.** The host provides ESM source as a
  string. V8 compiles and evaluates it inside the isolate; transitive
  `import`s resolve back through the same map. Zero per-call bridge cost.
- **Object value — host module.** The host provides a JS object whose
  shape becomes the module's exports. The runtime walks the object
  recursively and auto-generates ESM source: data leaves become JS
  literals, function leaves become async stubs that call a single
  **ID-addressed dispatcher**:

  ```js
  // generated for { search: fn } — fn assigned handle ID 0
  export const search = async (...args) => await globalThis.__iso4_call(0, ...args);
  ```

  `__iso4_call` is one ordinary host global whose handler peels the
  leading integer handle ID and routes to a per-run registry
  (`Map<id, fn>`). This means there are **no generated bridge-global
  names** — nothing to sanitise, no collision class (two specifiers like
  `host:tools` and `host_tools` can never clash because there are no
  derived names, only integer IDs). The wire format and the Rust side are
  unchanged: `__iso4_call` is dispatched like any other named global.
  Nested objects mixing data and functions are walked recursively and
  Just Work — `db.users.create()` is a single dispatcher call to one
  handle ID, no more expensive than a flat call.

  The same registry + dispatcher is the foundation for **callable handles**
  (Phase 13): functions *returned* from a bridge call get registered the
  same way and materialise into the same `__iso4_call` stubs, so
  request/response objects (`res.json()`) become purely additive.

```ts
imports: {
  // string → source module
  "lodash-es": lodashEsmBundle,

  // object → host module (auto-generated source + bridge globals)
  "host:tools": {
    search: async (q) => mySearch(q),
    version: "1.0",
    cfg: { mode: "prod", helpers: { greet: () => "hi" } }, // nested mix is fine
  },
}
```

Both flavors are submitted to V8 as standard ESM modules; the only
difference is who wrote the source string. The runtime resolver looks up
each specifier in `imports`. Anything missing surfaces as
`ERR_MODULE_NOT_FOUND`.

The object form imposes these restrictions on the value tree:

- **Function leaves** must take serialisable data and return serialisable
  data. In v1 the generated stub is always `async` because bridge calls
  cross a UDS round trip; the sandbox always `await`s the result.
  Synchronous delivery is Phase 11 (see the sync-codegen note below).
- **Function arguments may not themselves be functions** (no callback
  support in v1). Bridge rejects with `FunctionArgumentNotSupported`.
  Passing host functions back to the sandbox via *return values* is
  Phase 13 (callable handles).
- **Data leaves** must be representable as JS literals: primitives,
  strings, plain objects, arrays, `Date`, `BigInt`, `Uint8Array`.
  `Map` / `Set` / circular references throw at registration with a
  pointer to the eventual "lazy data load" mechanism, if it becomes
  needed.
- **Stateful object handles** (`createReadStream` returning a stream)
  remain unsupported; nothing changes there.
- **Class instances with prototype methods** — methods on the prototype
  are *not* discovered by the walker (own enumerable properties only).
  Copy methods onto the instance explicitly if you want them exposed.

Rebinding on `prefix.run()` is keyed on the same shape: only function
leaves declared at `precompile()` time may be rebound, and only with the
same signature. TypeScript enforces this via `RebindImports<I>`; the
runtime fallback for `as any` shenanigans returns
`ERR_UNDECLARED_BINDING` symmetrically with the globals path.

#### Implementation note: why generated source instead of synthetic V8 modules

An earlier version of Phase 7 used V8 synthetic modules
(`v8::Module::create_synthetic_module` + `set_synthetic_module_export`) to
bind host-module exports natively. Pre-v1 it was rewritten to the
source-generation approach because:

1. The public API simplifies dramatically (`Record<string, string |
   object>` instead of a discriminated union with `kind`, `exports`,
   `source` fields).
2. Nested objects mixing data and functions fall out for free — the
   recursive walker emits literal source with bridge-global references at
   the function leaves.
3. The implementation reduces to two primitives already present (source
   imports + globals) instead of a third (synthetic modules) carrying its
   own wire-format fields, callback shapes, and `Box` lifetime
   management.
4. Generated source is a real ESM module: debuggable, code-cacheable by
   V8, and present in stack traces under a sanitised filename.

The native-binding mechanism is preserved as **Phase 12** ("Native
host-module binding") if performance, stack-trace polish, or in-process
integration ever justify it. Until then the source-gen path is the
shipping implementation.

#### Implementation note: sync codegen (Phase 11)

The source generator decides per function leaf whether to emit an async
stub (`__iso4_call`, returns a Promise) or a sync stub (`__iso4_call_sync`,
blocks the isolate and returns the value directly):

```js
// async leaf
export const search = async (...args) => await globalThis.__iso4_call(0, ...args);
// sync leaf
export const readFileSync = (...args) => globalThis.__iso4_call_sync(1, ...args);
```

**How a leaf is classified.** Default by inspecting the host handler:
`fn.constructor === AsyncFunction` (the robust check). **Do not** use
`fn.toString()` parsing for the `async` keyword — it silently misclassifies
bound async functions, which stringify to `function () { [native code] }`
with the keyword stripped (verified). A sync handler (`() => x`) → sync
stub; an async handler (`async () => x`) → async stub.

**Important semantic caveats this classification does not capture, so an
explicit per-leaf override must remain available:**

- Detection only chooses the **sandbox-side calling convention** (Promise
  vs blocking value). It does not change host-side behaviour: the host
  dispatcher always `await`s the handler before replying, so an async
  handler can back a sync sandbox stub and vice versa. `readFileSync` in
  the sandbox can be backed by `async (p) => await fs.readFile(p)`.
- A sync function that returns a Promise (`() => Promise.resolve(1)`) is
  classified "sync" by the constructor check but behaves async. The host
  awaits the result regardless, so this still works; the only effect is
  the sandbox sees a blocking stub.
- Sync stubs **block the isolate** (no concurrency, no `Promise.all`
  parallelism, microtasks paused during the wait). That is the point of
  sync, but it means auto-classifying every plain arrow function as sync
  could surprise authors who wrote `() => x` without intending blocking
  semantics. **Open question for Phase 11:** is the constructor-based
  default the right policy, or should sync be strictly opt-in
  (`{ sync: true, handler }`) with async the default for everything? The
  CPU-budget bracketing and wall-clock preemption from Phase 3 already
  make blocking safe; the question is purely ergonomic.

Mechanically this is purely additive over today's design: a second
reserved global (`__iso4_call_sync`), a non-`async` generated stub for
sync leaves, and one blocking bridge callback in Rust. The handle
registry (`Map<id, fn>`), the ID assignment, and the walker are unchanged
— sync-ness is a property of the emitted *stub*, not of the registry
entry.

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

Every run resolves to one of three outcomes, discriminated by `status`:
`'completed' | 'failed' | 'aborted'`. `ok` is kept as a convenience alias for
`status === 'completed'`, so the common guard stays `if (result.ok)`; reach for
`switch (result.status)` when a deliberate abort must be told apart from a
genuine failure.

A completed run:

```ts
{ status: "completed", ok: true, exports: { … }, stdout, stderr, durationMs }
```

If user code throws (uncaught), or the runtime kills the isolate (memory, CPU,
wall), the run **fails**:

```ts
{
  status: "failed",
  ok: false,
  error: {
    name: "TypeError",
    message: "Cannot read property 'x' of undefined",
    code: "ERR_USER_CODE",  // or ERR_MEMORY_LIMIT | ERR_CPU_TIMEOUT | ERR_WALL_TIMEOUT | …
    stack: "...",
    fields: { … },  // all other own-enumerable props of the thrown error, if any
  },
  stdout: "...",  // whatever was emitted before the throw
  stderr: "...",
  durationMs: 42,
}
```

`name`/`message`/`stack` are reserved: always read from the error's dedicated
properties, never mixed into `fields` — so a thrown object's own `code` or
`stack`-named field can't collide with or spoof the top-level values. Thrown
primitives normalise to `{ name: "Error", message: String(value) }` with no
stack and no fields.

If the host aborts the run via `run({ signal })` — a pre-aborted signal at entry
or an abort that fires mid-run (see §14.7) — the run is **aborted**. This is a
deliberate outcome, not a failure, so it gets its own `status`. `error` is
retained (its `code` is always `ERR_ABORTED`) so existing `!result.ok` /
`error.code` checks keep working, and `reason` carries whatever value was passed
to `AbortController.abort(reason)`:

```ts
{
  status: "aborted",
  ok: false,
  error: { code: "ERR_ABORTED", name: "AbortError", message: "run was aborted" },
  reason,   // the value passed to abort(reason), or undefined
  stdout, stderr, durationMs,
}
```

The result is _always_ an object; `status` (or the `ok` alias) discriminates.
`run()` does not throw for sandboxed failures — only for infrastructure failures
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

`maxIsolates` in `SandboxOptions` controls the pool ceiling.
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
The short version:

```
iso4/
  DESIGN.md                         ← this file

  packages/iso4-sandbox/src/types.ts ← canonical API shapes for @iso4/sandbox
  packages/iso4-sandbox/src/types.ts    ← shared types (@iso4/sandbox)
  pnpm-workspace.yaml
  packages/
    iso4/                           ← the runtime: createSandbox, run, precompile
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
- `@iso4/sandbox/v8-runtime`: <3000 LoC Rust.
- JS injected into each isolate: <300 LoC.

---

## 9. Phased build plan

Reordered so the snapshot-based prefix mechanism lands early — it's the
feature most users will rely on and shapes the IPC protocol.

| Phase  | Scope                                                                                                                                     | Deliverable                                                                |
| ------ | ----------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------- |
| 0 ✅   | This doc + `packages/iso4-sandbox/src/types.ts` + `packages/iso4-sandbox/src/types.ts` + workspace scaffolding                            | API committed before code                                                  |
| 1 ✅   | Rust binary: spawn, UDS, single run, heap limit, CPU timeout (wall-clock), ESM compile + evaluate, captured console, export serialization | `runtime.run({ code, limits })` works end-to-end with no imports, no fetch |
| 2 ✅   | Precompile + `PrecompiledPrefix.run()` via V8 startup snapshots                                                                           | The canonical AI-agent prefix/postfix loop works                           |
| 3 ✅   | CPU budget enter/leave bracketing (async time exclusion)                                                                                  | Tight loops killed quickly; `await fetch` doesn't burn budget              |
| 4 ✅   | Generic host-bridge dispatch for globals (string / function / shimmed); `fetch` is just one allowed name on this path                     | Hosts can expose any allowlisted global; `fetch` works as a regular global |
| 5 ✅   | `@iso4/fetch` package: `createSafeFetch` with allowlist, DNS pin, private-IP blocking, no-auto-redirect                                   | Hardened default users can opt into in two lines                           |
| 6 ✅   | Imports: source modules (Flavor B); host-supplied ESM strings compiled per-isolate. No separate code-cache LRU — the precompile snapshot is the cache.   | `import { add } from "lib:math"` works when the host declares the source     |
| 7 ✅   | Imports: host modules — host provides a JS object; the runtime generates ESM source that exposes the object's structure (function leaves call bridge globals, data leaves are JS literals). Nested mixed objects supported via recursive walker. See §4.3. | `import { search } from "host:tools"` works for arbitrarily-nested mixed data/function shapes; the synthetic-module native-binding path is preserved as Phase 12 |
| 8     | Custom `ArrayBuffer` allocator, near-heap-limit graceful kill, hard wall-clock guard separate from CPU budget                             | Memory and time limits are tight under adversarial input                   |
| 9     | Pre-warmed isolate pool (optional, behind a runtime option)                                                                               | Sub-2ms cold start for high-throughput workloads                           |
| 10    | Polish: error types, integration tests, READMEs, examples                                                                                 | Shippable v1                                                               |
| 11    | **Sync bridge calls.** Per-leaf codegen emits a sync stub (`__iso4_call_sync`, blocks the isolate on UDS read) instead of the async stub. Leaf classification + the opt-in-vs-auto policy question are covered in the §4.3 "sync codegen" note. CPU budget bracketed; wall guard preempts via `terminate_execution`. | `import { readFileSync } from "host:fs"` works without `await`. Foundational for a future node-compat layer. |
| 12    | **Native host-module binding.** Replace the source-generation path (§4.3) with synthetic `v8::Module` export slots filled at evaluation. Recursive Rust walker, per-leaf `BridgeCall { targetKind: 1 }` (revived), `Box` lifetime registry. Supports both sync (from Phase 11) and async stubs. | Lower per-run overhead for import-heavy code; cleaner stack traces. No behaviour change visible to sandbox code. |
| 13    | **Callable handles for return values.** Functions crossing back from a bridge call get a per-run integer ID; sandbox invokes via `BridgeCall { targetKind: 2 }`. Host-side handle registry, GC on run end. | `await fetch().then(r => r.json())`, `cursor.next()`, any host-returned method callable from sandbox. |

Each phase is independently shippable. Phases 11–13 are post-v1; nothing
in Phases 1–10 needs to know about them.

**End of the two-process sandbox roadmap.** Everything beyond this line
belongs to separate products with their own design passes (see §9.1).

### 9.1 Future products (separate roadmaps, design TBD)

What used to be Phase 12 (session API on two-process) and Phase 13 (in-process
NAPI backend behind a `SandboxOptions.backend` flag) have been removed from
the `@iso4/sandbox` plan. The two-process backend cannot reach the
call-overhead target that an analytics use case needs, and pretending one
TypeScript API spans both transports hides real design decisions (shared
`ArrayBuffer`s, threading model, lifecycle, type surface).

The iso4 ecosystem after Phases 1–13 looks like this:

- **`@iso4/sandbox`** — the two-process AI-agent product. Done at Phase 13.
- **An analytics runtime** (package name TBD) — a separate product designed
  after `@iso4/sandbox` is finished.
- **`@iso4/node-stdlib`** (or similar) — optional add-on packaging curated
  `fs` / `path` / `process` / etc. as host imports. Sits on top of sync
  bridge calls (Phase 11) and callable handles (Phase 13).

#### Analytics runtime

Deliberately not specified now. What is known:

- **In-process** (NAPI / napi-rs). No UDS, no auth, no subprocess.
- **Shared `ArrayBuffer`s / typed-array views** across host ↔ sandbox
  without copy. Per-row analytics cannot afford copy cost.
- **Sub-µs amortized call overhead.** Two-process cannot reach this with
  any amount of tuning; this is the reason analytics is its own product.
- **Persistent isolate, sequential calls.** Different lifecycle from
  `@iso4/sandbox`'s pool of one-shot isolates.
- **Outer isolation boundary is the container** (Docker / K8s). The
  sandbox provides memory-safety + API curation, not adversarial-code
  containment.

What is shared with `@iso4/sandbox`:

- The wire-frame types (`WireValue`, the bridge payload conventions).
- The "only data crosses" rule and `ValueSerializer`-based contract.
- The host-import declaration shape (`Imports<…>` from §4.3) — the
  developer experience of describing the bridge surface stays uniform.
- The limits semantics and error vocabulary.

What is **not** shared:

- The runtime API. `Sandbox` / `Prefix` / `prefix.run()` solves a different
  problem than analytics' eventual `Pipeline` / `Session` / `processBatch`.
- The transport.
- The lifecycle and concurrency model.

Open design questions that can only be answered once Phases 1–13 ship:

- How does the host hand row batches to the sandbox without copy? Shared
  `ArrayBuffer` + a typed schema description? Apache Arrow? A custom
  zero-copy view layer?
- What replaces `prefix.run()` for an analytics loop —
  `pipeline.process(batch)`? `session.call(fn, args)`?
- Are bridge calls even the right primitive, or does direct V8 function
  pointer invocation (possible in NAPI) replace them?
- Where does the type-level surface live — same package as the sandbox,
  or a sibling `@iso4/analytics`?

These will be resolved in their own design pass, not retrofitted onto
`@iso4/sandbox`.

---

## 10. Open questions

To be resolved as we build, not blocking the start:

- **Single-isolate-per-process or multi-isolate?** Resolved: **multi-isolate
  from the start**. The Runtime manages a connection pool; each slot has its
  own UDS connection and its own isolate thread in the Rust process. Pool
  size = `maxIsolates` (default: CPU count). This is required for MCP
  multi-agent parallelism.

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
  Configurable via `SandboxOptions.maxPrecompiledPrefixes`. Evicted handles
  fail `.run()` with `ERR_PREFIX_DISPOSED`; the host re-precompiles.

- **Mandate `createSafeFetch` or recommend it?** The core `iso4` package's
  `fetch` field accepts any `FetchHandler`. The library does mechanical
  hygiene (header/URL validation) at the bridge; the host author decides
  policy (allow/deny). `@iso4/fetch` ships hardened defaults but is opt-in.
  Documented strongly.

- **`Session.call()` input/output serialization contract.** For the
  persistent-session API of the analytics product, the host calls a function that was
  exported by the prefix. What can be passed as `input`? V8 `ValueSerializer`
  is the natural choice (same as exports today), but typed arrays could be
  transferred zero-copy if needed for bulk data. Decide when designing the analytics product (§9.1).

- **In-process backend: C++ NAPI addon or Rust NAPI with Node's V8
  headers?** `rusty_v8` cannot be used in-process because it calls
  `v8::V8::initialize_platform()`, which Node already did. A C++ NAPI
  addon (like `isolated-vm`) is the proven path. Rust via raw `bindgen`
  to Node's V8 headers is possible but undocumented territory. Decide
  when the analytics product is designed (§9.1).

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
eviction policy, and warmup-on-precompile are all `SandboxOptions`.

Not in v1. Snapshots alone get steady-state cold start under 5 ms which is
plenty for interactive agents. The pool is for sub-millisecond requirements.

## 12. Security model — fetch hardening

This section codifies the responsibility split between the library and the
host author. The short version: the **library** must do mechanical hygiene
that no one would get right at the application layer; the **host author**
must decide policy. The library cannot decide policy. The host author
should not have to worry about CRLF injection.

### 12.1 What actually happens when sandbox JS calls a host-provided global

Host-provided globals (`fetch`, `myTool`, or any other non-reserved name)
are installed as bridge stubs in the V8 context. When sandbox code calls
one, the call is serialized as plain data via V8 `ValueSerializer`, Rust
sends a `BridgeCall` frame with the function name and arguments, the
TypeScript host runs the configured handler for that name, and the result
is passed back via `BridgeResponse`.

**The bridge is fully generic.** It has no knowledge of fetch semantics,
URL parsing, or HTTP. `fetch` is just a string key the host chose to
register. If the host registers `{ fetch: myFn }`, then `myFn` is called
with whatever V8-deserialized arguments the sandbox passed.

**The bridge does NOT auto-route to `globalThis.fetch` on the host.** If the
host author writes a handler that calls `globalThis.fetch`, then yes, the
host's real network stack runs with sandbox-controlled inputs. If they write
something restrictive, that runs. The bridge is just a function-call protocol.

This means `FetchHandler`, `HostFetchRequest`, and `HostFetchResponse` are
**not core types**. They are convenience types defined in `@iso4/fetch` that
describe the expected call shape for a `fetch`-compatible handler. Core only
knows `HostExportFunction` — a function that takes `HostExportData` arguments
and returns `HostExportData`.

**When the handler throws or rejects**, the sandbox call rejects with a real
`Error` rebuilt from the thrown value: same `name` (built-in names use the
matching intrinsic constructor, so `instanceof TypeError` works), same
`message`, and every other own-enumerable property re-attached as a direct own
property (`e.status`, `e.reason`, …). The **host stack never crosses** into
the sandbox — it can expose host file paths and infrastructure details — and
`name`/`message`/`stack` cannot be injected through the carried fields.
Sandbox code can catch the error and continue running; if it stays uncaught it
fails the run as `ERR_HOST_BRIDGE` with the same identity preserved on
`RunError` (extra props under `error.fields`). Bridge *limit* violations
(`maxBridgeCalls`, `maxBridgeCallBytes`, function arguments) are different:
they stay fatal to the run even when caught.

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
  - Generic host-bridge plumbing for globals and imports. The bridge
    serializes call arguments and return values as `HostExportData` in both
    directions. No fetch-specific logic anywhere in the core.
  - `HostGlobals` is `Record<string, HostExportFunction>` — any non-reserved
    name, any bridge function.

- **`@iso4/fetch`** ships:
  - `FetchHandler`, `HostFetchRequest`, `HostFetchResponse` — typed
    convenience wrapper for the fetch call shape.
  - `createSafeFetch({ allowedHosts, blockPrivateIPs, maxRedirects, timeout, ... })`
    returning a `HostExportFunction`-compatible handler that does all the
    mechanical hygiene before calling the real network.
  - DNS pre-resolution with IP pinning.
  - Isolated `undici` Dispatcher (no shared connection pool with the host).
  - Per-host rate limiting hooks.

- **Host author** writes:
  ```ts
  import { createSandbox } from "@iso4/sandbox";
  import { createSafeFetch } from "@iso4/fetch";

  const runtime = await createSandbox();
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

## 13. Execution model

`@iso4/sandbox` ships exactly one execution model: **one-shot runs**.

```
prefix.run({ code }) → RunResult
```

The isolate is created from the snapshot, executes a code string, returns
named exports, and is torn down. Every run is independent. IPC cost: one
round trip (Run → … → Result). Suitable when the work per run is
non-trivial and async (fetch, host imports). This is the AI-agent
prefix/postfix pattern described in §11.

### 13.1 Persistent sessions are a separate product

Earlier revisions of this document specified a second execution model in
`@iso4/sandbox` — `prefix.openSession()` + `session.call(fn, input)` for
persistent isolates — plus an in-process NAPI backend behind a
`SandboxOptions.backend` flag. Both have been **removed from the
`@iso4/sandbox` plan** (see §9.1).

The short version: the call-overhead target that motivates persistent
sessions (“per-row analytics”) is unreachable over UDS, and pretending one
TypeScript API spans both transports hides real design decisions —
shared `ArrayBuffer`s, threading model, lifecycle, the type surface.

The analytics use case is its own product with its own design pass,
built after Phases 1–13 ship. It will share wire-frame types,
`ValueSerializer` contracts, limits vocabulary, and the `Imports<…>`
declaration shape with `@iso4/sandbox`, but it will not be a backend
flag on `createSandbox`.

When the analytics product is designed, the comparison below is what it
needs to beat:

|                        | `@iso4/sandbox` (this product) | Analytics runtime (future)  |
| ---------------------- | ------------------------------ | --------------------------- |
| Call shape             | `prefix.run({ code })`         | TBD (`pipeline.process`?)   |
| Isolate lifetime       | per-call                       | persistent                  |
| Async / fetch          | ✅                             | TBD                         |
| Imports (host modules) | ✅                             | shared declaration shape    |
| Transport              | UDS subprocess                 | NAPI / napi-rs in-process   |
| Memory model           | copy via `ValueSerializer`     | shared ArrayBuffer + schema |
| Per-row analytics      | impractical                    | the whole point             |
| Crash isolation        | ✅ always                      | container is the boundary   |
| Outer security model   | subprocess                     | Docker/K8s                  |

### 13.2 Historical session-API sketch (archived; not implemented)

The paragraphs below are kept as **design notes for the future analytics
product**. They are not part of `@iso4/sandbox` and the wire-protocol
opcodes mentioned (`OpenSession`, `Call`, `CloseSession`,…) are not
allocated in this codebase.

The original session sketch:

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
backend. The backend is selected at `createSandbox()` time via
`SandboxOptions.backend`:

```ts
// Two-process (default, v1)
const runtime = await createSandbox()

// In-process (Phase 12, opt-in)
const runtime = await createSandbox({ backend: 'inprocess' })
```

Both produce the same `Runtime`, `PrecompiledPrefix`, and `Session` objects.
The one-shot `prefix.run()` also works with the in-process backend (it
creates a session, runs the code as the session's body, returns exports,
closes the session).

### 13.4 Choosing between the models (archived)

No choice to make in this product — `@iso4/sandbox` ships only one
execution model. The table below is the original comparison, kept for
the future analytics design pass.

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

---

## 14. Bridge call constraints and limitations

This section documents the observable behaviour and known constraints of the
host bridge. These are design decisions, not accidents.

### 14.1 Sequential bridge calls (v1)

Bridge calls are **sequential within a single run**: Rust sends one `BridgeCall`
and blocks until the matching `BridgeResponse` arrives before JS resumes. At
most one handler is active at a time per run. Concurrency across runs is fully
parallel (each run owns its own connection slot); it is only within one run
that bridge calls are serialised.

This is sufficient because the sandbox is itself single-threaded, so there can
only be one pending `await someGlobal()` at a time. A future phase (13+) may
add intra-run multiplexing if needed for very deep async call trees.

### 14.2 No callable return values (current)

Bridge return values are plain data (`WireValue`). Functions on returned
objects are **silently dropped** — the sandbox receives the plain data fields
but no methods. This means the handler cannot return a `Response`-like object
with `.json()` on it today.

**Workaround**: design globals as high-level tools that return already-processed
data:

```ts
// Good: tool returns processed data
globals: {
  searchWeb: async (q: string) => { const r = await fetch(`/search?q=${q}`); return r.json() },
}

// Bad: trying to mirror web-standard fetch API
globals: {
  fetch: async (url: string) => ({ status: 200, body: '...' }),
  // sandbox must JSON.parse(res.body) — no .json() method
}
```

Callable return values are planned in Phase 13 (callable handles). See §15.

### 14.3 `prefix.run()` globals are rebind-only

`prefix.run({ globals })` may only **rebind** globals declared at
`precompile({ globals })` time. Adding a new name at run time results in
`ERR_UNDECLARED_BINDING` at runtime and a TypeScript compile-time error (when
the `Prefix<G>` type parameter is specific).

Rationale: the snapshot captures the prefix’s heap shape. Silently installing
an undeclared global into a restored snapshot context would mutate that shape
in a way the snapshot doesn’t know about.

Omitting a declared global at run time is allowed — the precompile-time
default implementation is reused.

### 14.4 Handler lifecycle when wall timeout fires

When the run’s wall-clock budget expires while the sandbox is blocked inside a
bridge call:

1. **Rust side**: the socket read in the bridge callback has a read timeout set
   to the remaining wall budget. When it fires, the callback explicitly calls
   `terminate_execution()` on the isolate handle, sets `TerminationReason::Wall`,
   and returns without a result. V8 terminates at the next safe point.
2. **TypeScript side**: the `drainUntilResult` loop races each dispatcher
   invocation against a timer equal to the remaining wall budget + 200 ms.
   When the timer wins, the loop skips writing a `BridgeResponse` (which would
   corrupt the next run’s session) and reads the `Result { ERR_WALL_TIMEOUT }`
   frame that Rust sent.
3. **Handler promise**: the host handler’s `Promise` is **orphaned** — it has
   no listener and will be garbage-collected when it eventually settles. Any
   in-flight I/O or timers inside the handler continue until they complete
   naturally. Node.js cannot forcefully kill a live `Promise`.

**Consequence**: if the handler starts a network request and the wall timeout
fires, the network request continues in the background after the sandbox run
has already returned. This is generally acceptable for short-lived handlers.
For long-lived handlers (e.g. ones that open DB connections), the host author
should design them to be safe to orphan.

### 14.5 AbortSignal for handler self-cancellation (planned)

In a future phase the bridge dispatcher will receive an `AbortSignal` that is
aborted when the run terminates (wall timeout, explicit `.dispose()`, etc.).
Handlers that accept the signal can cancel in-flight I/O cleanly:

```ts
// Future API (not implemented yet)
globals: {
  fetchData: async (url, { signal }) => {
    const res = await globalThis.fetch(url, { signal })
    return res.text()
  },
},
```

Until that lands, handlers should be written to be safe to orphan.

### 14.6 Function arguments rejected

Passing a function as an argument to a bridge global is rejected with
`ERR_FUNCTION_ARGUMENT_NOT_SUPPORTED`. Only serialisable data values
(`WireValue`) may cross the boundary in either direction in v1.

This is a deliberate limitation — callbacks across the boundary would require
a callback-handle protocol symmetric to callable return values. The same Phase
11 work that adds callable return values will enable function arguments too.

### 14.7 In-flight run abort via `AbortSignal`

`run({ signal })` and `prefix.run({ signal })` honor an `AbortSignal` that
fires **at any point during a run**, not just at entry. A pre-aborted signal
short-circuits to `ERR_ABORTED` before any frame is sent; an abort that lands
mid-run tears the run down promptly.

Mechanism (TypeScript-only — no Rust control message is required):

1. **Interrupt the drain**: `drainUntilResult` subscribes to the signal. On
   abort it closes the connection's frame reader (so the read loop throws) and
   destroys the socket.
2. **Reclaim the isolate**: destroying the socket makes the Rust session's
   blocking bridge-response read observe EOF; it returns an error, `run_code`
   unwinds, and the isolate is dropped — promptly, rather than waiting for
   `wallTimeMs`.
3. **Drop the late response**: an orphaned bridge handler that resolves *after*
   the abort tries to write its `BridgeResponse` to a socket that is already
   gone; the write fails silently. The sandbox therefore never observes a
   return value for the call that was in flight when the abort landed. This
   makes `controller.abort()` from inside a bridge handler a spoof-proof way to
   stop a run: sandbox code cannot catch or swallow it, and no error name has
   to propagate through the isolate. (This is the mechanism the
   `durable-isolates` replay kernel uses to implement durable suspension.)
4. **Keep the pool healthy**: a connection torn down this way is marked unusable
   and is **not** returned to the free list. `ConnectionPool` disposes it and
   opens a fresh replacement, so the pool keeps its full `maxIsolates`
   complement and other/subsequent runs are unaffected.

The `run()` promise resolves with `status: 'aborted'` (see §5.2) in all cases —
carrying the value passed to `abort(reason)`, and retaining `error.code:
'ERR_ABORTED'` for backward compatibility.

**CPU-bound caveat**: a purely CPU-bound run (no bridge call in flight) is
spinning inside `module.evaluate()` and does not observe the socket close.
The `run()` promise still resolves as aborted immediately, but the abandoned
isolate is only reclaimed when its **CPU guard** fires — bounded by
`cpuTimeMs`, not `wallTimeMs`. Promptly interrupting a busy isolate would
require a Rust-side `terminate_execution` triggered by a control message; this
is deferred until a consumer needs it.

---

## 15. Callable handles (Phase 13)

### 14.1 The problem

Bridge return values are plain data (`WireValue`). This means a host handler
that wants to return a rich object with methods — the classic example is a
fetch `Response` with `.json()` and `.text()` — cannot do so: functions are
not serialisable. Currently the handler must return a flat data object and
the sandbox must do its own parsing (e.g. `JSON.parse(res.body)`).

The practical workaround is to design globals as high-level tools that return
already-processed data, rather than trying to mirror web-standard APIs:

```ts
// Preferred: tool returns processed data directly
globals: {
  searchWeb: async (query: string) => (await fetch(`...${query}`)).json(),
  readSecret: async (key: string) => vault.get(key),
}

// Avoid: trying to mirror raw Response API in sandbox
globals: {
  fetch: async (url: string) => ({ status: 200, body: '...' }),
  // sandbox must then JSON.parse(res.body) manually
}
```

For the web-standard `fetch` use case specifically, the host is expected to
wrap `fetch` in a global that either returns processed data directly or
returns a shape the agent can work with naturally. Phase 13 lifts this
restriction for cases where the host genuinely needs to return objects with
methods.

### 14.2 Mechanism: per-run callable handle table

The host handler returns a value that contains functions. The TypeScript
serialiser detects functions in the return value and replaces each one with
a plain-data marker `{ __iso4_fn__: <id> }` before encoding as `WireValue`.
The original function is stored in a per-run `callableHandles` map keyed by
the ID. The map is released when the run completes — no explicit disposal
needed, no GC complexity.

```
TS handler returns:
  { status: 200, body: '{...}', json: [Function], text: [Function] }

Serializer produces:
  { status: 200, body: '{...}',
    json: { __iso4_fn__: 42 },
    text: { __iso4_fn__: 43 } }

callableHandles = { 42: originalJsonFn, 43: originalTextFn }
```

### 14.3 V8 side: callable stubs

`wire_to_v8_value` (Rust) detects `{ __iso4_fn__: <id> }` objects and
installs a real V8 `Function` in their place. The function's External data
is the same `GlobalCallbackData` as all other bridge stubs (same
`stream_fd`, `cpu_budget`, `call_id`, `bridge_error`), plus the callable ID.

When the sandbox calls the function (e.g. `await res.json()`), a new
`BridgeCall` is sent with `targetKind = 2` (callable) instead of
`targetKind = 0` (global) or `1` (import). The payload carries the callable
ID instead of an export name:

```text
BridgeCallPayload (targetKind = 2):
  u32  callId       — monotonic, same counter as global bridge calls
  u8   targetKind   — 2 = callable
  u32  callableId   — handle from the per-run table
  List<WireValue>  args
```

CPU budget bracketing (`leave()` / `enter()`) applies exactly as for global
calls: host-side execution time does not count against the sandbox budget.

### 14.4 TypeScript side: dispatch and cleanup

The `drainUntilResult` loop in `client.ts` gains a `targetKind === 2` branch:

```ts
case 2: {  // callable ref
  const fn = callableHandles.get(call.callableId)
  if (fn === undefined) {
    // unknown handle — send ERR_HOST_BRIDGE response
  } else {
    const result = await fn(...call.args)
    // encode and send BridgeResponse
  }
  break
}
```

The `callableHandles` map is created at the start of each run alongside the
dispatcher, and discarded when the run's `Result` frame arrives.

### 14.5 Nested callables

The result of a callable method call is itself encoded via `encodeWireValue`.
This means a callable can return another callable, and the sandbox can chain
method calls:

```js
// sandbox code
const conn = await db.connect()
const rows = await conn.query('SELECT * FROM users')
await conn.close()
export default rows
```

Each step is one additional bridge round-trip. Deep chains add latency but
work correctly — callables returned from callables get fresh IDs in the same
per-run table.

### 14.6 What callable handles are not

- **Not a general object-capability system.** Handles are one-shot
  per-run and cannot be passed to other runs or serialised into exports.
  `export default someHandleObject` will export the plain marker
  `{ __iso4_fn__: 42 }`, not a live stub — the export validator rejects
  it (or strips it silently, TBD).
- **Not a replacement for well-designed globals.** For most AI-agent use
  cases, a flat API that returns processed data (`searchWeb`, `readFile`,
  `queryDB`) is cleaner than returning method-bearing objects. Callable
  handles exist for cases where the returned shape is out of the host
  author's control (e.g. wrapping an existing library that returns a cursor
  or a stream handle).
- **Not persistent across runs.** Two `prefix.run()` calls cannot share
  callable handles. Each run has its own isolated handle table.
