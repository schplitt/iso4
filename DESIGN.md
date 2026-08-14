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

The prefix is validated once at `prepare()` and cached (source + declared
shape); every execution evaluates it into a fresh isolate and then runs a
function, returning its result.
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

`prefix.run({ code })` handles this. A fresh isolate is created per call
and the prefix is re-evaluated into it, so each caller starts from a clean
state regardless of what previous callers did. Multiple callers running simultaneously each get
their own pool slot and run in parallel.

**Static code** — the function is compiled into the prefix and does not
change between calls. Only the input data varies. The prefix exports the
function by name; the host calls it with each new input:

```js
// Prefix code (validated once at prepare())
export async function transform(row) {
  return { revenue: row.price * row.qty }
}
```

The static-code shape above is achievable in `@iso4/sandbox` via
`prefix.run()` with a per-run host import that provides the input data —
at the cost of a fresh isolate + prefix evaluation per call. A persistent-session API
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
   as a V8 serialization blob, which restricts results to data
   (no functions, no methods — see §5.1).
8. **Captured stdout/stderr** via a runtime-owned `console`.
9. **Pre-validated prefix code**: `prepare()` validates the prefix once;
   each run re-evaluates the cached source (sub-ms for typical prefixes;
   grows with prefix size). Runtime startup snapshots were removed — see
   §11.6.
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
  the prefix is re-evaluated into — the _prefix's declared shape_ is shared,
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
  │      └─ stock V8 snapshot for sub-ms isolate boot       │    
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
5. Rust evaluates the module. Top-level `await` works. (In a prefix at
   `prepare()` time it works as long as it settles without host I/O — see
   §11.3.)
6. Once evaluation settles, Rust copies the module namespace into a plain
   object and serializes it **once** as a V8 blob — `default` and every named
   export in one payload. Functions, promises, symbols, `WeakMap`s, and
   proxies throw `ExportNotSerializable`; everything V8's format carries
   (`Date`, `Map`, `Set`, `RegExp`, `Error`, typed arrays, cycles) crosses as
   a real instance (see §5.1).
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
| Result serialization                | V8 serialization blob (Rust side), data only          |

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

**Export size** (`maxExportBytes`): Rust serializes all exports into one V8
blob and measures that blob. If it exceeds `maxExportBytes` the run fails with
`ERR_EXPORT_TOO_LARGE` before the `Result` frame is written. The check is on
the bytes that actually cross the socket, so it costs nothing extra.

**Stdout/stderr sizes** (`maxStdoutBytes`, `maxStderrBytes`): Rust tracks
running byte totals during console capture. Any line whose addition would
push the total over the configured cap is silently dropped; the run continues
normally. Zero disables the cap.

### 4.2 Globals (block-listed, not allowlisted)

Globals are _not_ a free-for-all. The runtime owns a fixed set of reserved
names that the host must not shadow:

- `console` — owned by the runtime for output capture.
- The web runtime the runtime installs: `Headers`, `Request`, `Response`,
  `TextEncoder`, `TextDecoder`, `URL`, `URLSearchParams`.

These are **enforced**, not merely documented: a host global using one of these
names is rejected with `ERR_UNDECLARED_BINDING`. Allowing a host to shadow
`Response` would leave user code building objects the codec cannot recognise.

> **Correction (this section previously claimed otherwise).** These were
> listed here as "V8 built-ins" alongside `crypto`, `Event`,
> `AbortController` and `AbortSignal`. They are not. Plain V8 provides none
> of them — they are embedder-supplied in Node, Deno and browsers, and a bare
> iso4 sandbox had **no** web globals at all beyond `console`. `Headers`,
> `Request`, `Response`, `TextEncoder`, `TextDecoder`, `URL` and
> `URLSearchParams` are now installed by the runtime (see §4.4 below);
> `crypto`, `Event`, `AbortController` and `AbortSignal` still do not exist
> and are **not** reserved.

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

### 4.2.1 The web runtime

The sandbox ships a minimal, deliberately incomplete web runtime so that
request/response-shaped code can run and so `Request`/`Response` can cross the
boundary as real objects rather than flattening.

Installed: `Headers`, `Request`, `Response`, `TextEncoder`, `TextDecoder`,
`URL`, `URLSearchParams`.

**Implementation.** Each of the three serializable classes is a JS class
extending a native `FunctionTemplate` shell whose instance template declares one
internal field. The field is what makes serialization work: V8 routes objects
with internal fields to `WriteHostObject` off a map field read, leaving
`HasCustomHostObject()` `false` so no embedder callback fires for ordinary
objects. workerd depends on the same property. Behaviour above the shell is JS,
evaluated into every run's context at creation (~0.5 ms, measured).

**Deliberate deviations from spec**, in scope terms rather than bugs:

- No `.body` getter. That is a `ReadableStream`, and streams cannot cross a
  one-shot boundary (`docs/protocol.md` §4.4.5). Use `text()`, `json()`,
  `arrayBuffer()`, `bytes()`.
- No `Blob` or `FormData`, so no `blob()`/`formData()` and neither works as a
  body initializer.
- `URL` is a pragmatic parser: correct for http(s)/ws(s)/ftp/file and relative
  resolution, without IDNA/punycode or non-special-scheme edge cases.
- No `crypto`, `AbortController`, `AbortSignal`, `Event`, `structuredClone`,
  `setTimeout`, `queueMicrotask`.

Widening this set is additive and does not change the wire format
(`docs/protocol.md` §4.4), which is why the tier was chosen deliberately rather
than aiming at full compliance up front.

### 4.3 Imports

The full extension point. `imports` is a flat map keyed by specifier; the
value type is the discriminator:

- **String value — source module.** The host provides ESM source as a
  string. V8 compiles and evaluates it inside the isolate; transitive
  `import`s resolve back through the same map. Zero per-call bridge cost.
- **Object value — host module.** The host provides a JS object whose
  shape becomes the module's exports. The client walks the object
  recursively and ships the **shape as plain data** over the wire (#37):
  function leaves as bare markers, data leaves as V8 value blobs, nested
  objects as trees. No JS source is ever generated from the data. The
  Rust runtime builds the module natively:

  ```js
  // Rust-emitted module for { search: fn } — a fixed-shape template.
  // Only identifier-validated export names and integer indices appear;
  // the values array rides on the module's import.meta, populated by a
  // native V8 callback.
  export const search = import.meta.__iso4[0];
  ```

  Data leaves are materialised with the value codec (`blob::deserialize_value`);
  each function leaf becomes an async trampoline built by a fixed factory
  — `(id) => (async (...args) => await globalThis.__iso4_call(id, ...args))`
  — with its **handle ID passed as a number**, never printed into source.
  Handle IDs are assigned by the runtime in tree-walk order over the
  declared bindings, and the runtime owns the `id → (specifier, path)`
  table: when a trampoline fires, the dispatcher resolves the ID before
  the `BridgeCall` frame is written, so the frame (and the bridge-call
  record) carries the real `tools:search.query`-style name and the IDs
  never leave the runtime. The TS client routes the call through a
  location-keyed handler map (`(specifier, path) → fn`).

  `__iso4_call` is a reserved bridge stub the runtime installs itself
  whenever the declared imports contain a function leaf. There are **no
  generated bridge-global names** — nothing to sanitise, no collision
  class. Nested objects mixing data and functions are walked recursively
  and Just Work — `db.users.create()` is a single dispatcher call to one
  handle ID, no more expensive than a flat call.

  The same dispatcher is the foundation for **callable handles**
  (Phase 13): functions *returned* from a bridge call get registered the
  same way and materialise into the same trampolines, so
  request/response objects (`res.json()`) become purely additive.

```ts
imports: {
  // string → source module
  "lodash-es": lodashEsmBundle,

  // object → host module (shape shipped as data; Rust builds the module)
  "host:tools": {
    search: async (q) => mySearch(q),
    version: "1.0",
    cfg: { mode: "prod", helpers: { greet: () => "hi" } }, // nested mix is fine
  },
}
```

Both flavors materialise as standard ESM source-text modules inside the
isolate; the host-module template is Rust-emitted and contains no
host-provided text. The runtime resolver looks up each specifier in
`imports`. Anything missing surfaces as `ERR_MODULE_NOT_FOUND`.

The object form imposes these restrictions on the value tree:

- **Function leaves** must take serialisable data and return serialisable
  data. In v1 the trampoline is always `async` because bridge calls
  cross a UDS round trip; the sandbox always `await`s the result.
  Synchronous delivery is Phase 11 (see the sync-leaves note below).
- **Function arguments may not themselves be functions** (no callback
  support in v1). Bridge rejects with `FunctionArgumentNotSupported`.
  Passing host functions back to the sandbox via *return values* is
  Phase 13 (callable handles).
- **Data leaves** must be representable in V8's serialization format — see
  `HostExportData` and §5.1 for the set. They are **not inspected at
  registration**: the value goes straight to the serializer, which is the
  single gate on what may cross. A pre-walk would duplicate the serializer's
  work on the Node main thread at O(values), and any hand-maintained allowlist
  would drift from V8's real capabilities. Unsupported values therefore fail
  with the serializer's own data-clone error when the payload is encoded — still
  before anything reaches the sandbox, but without a path annotation.
  One consequence: a **cycle through a plain object** still throws, because the
  shape walker must descend plain objects to find function leaves and cannot
  tell nested shape from cyclic data. Cycles inside any other container
  (arrays, `Map`, `Set`, class instances) cross fine.
- **Stateful object handles** (`createReadStream` returning a stream)
  remain unsupported; nothing changes there.
- **Class instances with prototype methods** — methods on the prototype
  are *not* discovered by the walker (own enumerable properties only).
  Copy methods onto the instance explicitly if you want them exposed.

Rebinding on `prefix.run()` is keyed on the same shape: only function
leaves declared at `precompile()` time may be rebound, and only with the
same signature. TypeScript enforces this via `RebindImports<I>` at compile
time; at run time the client sends the rebind **locations**
(`specifier` + leaf path) on the `PrefixRun` payload and the Rust runtime
validates them against the shape stored with the prefix, returning
`ERR_UNDECLARED_BINDING` for anything else — one enforcement point,
shared with the undeclared-globals check, that a non-TS client cannot
skip.

#### Implementation note: how host modules are built (shape-as-data, #37)

Two earlier iterations informed the current design:

1. **Synthetic V8 modules** (original Phase 7):
   `v8::Module::create_synthetic_module` binds exports natively, but the
   synthetic module object holds a native evaluation-steps pointer. Any
   prefix that makes the module reachable from the snapshot heap (a
   namespace stash, a closure over an imported binding) would then
   require V8 external-reference bookkeeping on snapshot creation *and*
   every restore — a crash, not an error, when missed.
2. **Client-side source generation**: the TS client walked the object and
   emitted ESM text (function leaves as `__iso4_call(<id>, …)` stubs,
   data leaves as printed JS literals). Snapshot-safe, but emitting code
   from data is an injection-adjacent surface, and Rust only ever saw
   opaque generated source — bridge records needed TS-side name
   resolution.

The shipping design keeps the best of both: the shape crosses the wire as
**plain data**, and the Rust runtime emits only a fixed template
(`export const <name> = import.meta.__iso4[<i>];` — identifier-validated
names, integer indices) while building every value natively and handing
the values array to the module through V8's import-meta callback. The
module is an ordinary source-text module, so the prefix's host-module
bindings are plain JS with no native pointers — they re-evaluate cleanly
in every run; and the
runtime owns the handle table, so bridge frames and records carry fully
resolved names.

#### Implementation note: sync function leaves (Phase 11)

The wire shape tags each function leaf async or sync, and the runtime
picks the matching trampoline factory: async (`__iso4_call`, returns a
Promise) or sync (`__iso4_call_sync`, blocks the isolate and returns the
value directly):

```js
// async leaf trampoline (built by the async factory)
(async (...args) => await globalThis.__iso4_call(id, ...args))
// sync leaf trampoline (built by the sync factory)
((...args) => globalThis.__iso4_call_sync(id, ...args))
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
reserved dispatcher stub (`__iso4_call_sync`), a second trampoline
factory, and one blocking bridge callback in Rust. The handle table, the
ID assignment, and the shape walker are unchanged — sync-ness is a
property of the *trampoline*, not of the handler map entry.

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
  durationMs: 142,      // wall-clock, measured in the runtime
  cpuTimeMs: 12.4,      // active V8 execution; bridge waits excluded
  bridgeCalls: [        // recorded in the runtime; metadata only, never payloads
    { name: "fetch", startMs: 0.4, durationMs: 2.3, argBytes: 180, responseBytes: 41208, ok: true, blocked: false },
  ],
}
```

### 5.1 What can be exported

The boundary carries **data, not behavior**. Values travel as V8
serialization blobs (`docs/protocol.md` §4), so anything V8's own format can
represent crosses as a **real instance**:

- Primitives: `undefined`, `null`, booleans, numbers, strings
- `BigInt` (arbitrary precision)
- `Date`, `Map`, `Set`, `RegExp`, `Error` (and its subclasses)
- `ArrayBuffer`, every `TypedArray`, `DataView` — element type preserved, and
  a `subarray` window carries only its window
- Arrays (including sparse ones) and plain objects
- Cyclic and shared references — object identity survives the round trip

Behavior does not cross. These cannot be carried:

- Functions and classes
- Promises (must be `await`ed before exporting)
- Symbols
- `WeakMap` / `WeakSet` / `Proxy`

How a refusal surfaces depends on the position (#58):

- **Exports are skipped, never fatal.** An export whose value cannot cross —
  directly (`export default () => {}`) or nested
  (`export default { fetch }`) — is simply **absent** from `exports`, with
  its name reported in `skippedExports` so nothing is silently hidden. The
  whole offending export is dropped (whole-export skip, no partial pruning);
  sibling exports keep crossing. This is what makes
  `export default { fetch }` a first-class module shape: a plain `run()` on
  it reads the module's declaration exports instead of failing.
- **Everything else stays loud**: a call's return value (§5.3) and bridge
  values report `ERR_EXPORT_NOT_SERIALIZABLE` (sandbox → host) or a
  `TypeError` in the host encoder / `ERR_HOST_BRIDGE` (host → sandbox),
  since there the value *is* the result and skipping would mean silently
  returning nothing.

The same value contract applies in both directions — bridge call arguments,
bridge return values, host-module data leaves, call arguments/results, and
exports.

Two behaviours worth knowing:

- **Class instances flatten silently** to their own enumerable properties
  (`new Tenant()` → `{ id: "t1" }`; methods and prototype are gone). Node's
  serializer offers no hook to reject them, so this is an accepted trade-off
  rather than an oversight — copy what you mean to send into a plain object.
- **`"__proto__"` as an own key crosses as a plain own key.** It is defined,
  never `[[Set]]`, so the receiving object's prototype is untouched and no
  prototype pollution is possible.

Note for hosts: Node `Buffer` is a `Uint8Array` subclass, so it crosses
fine — but it always comes back as a plain `Uint8Array` (data preserved,
subclass identity not).

The **typed** contract matches the runtime one: `HostExportData` describes
exactly the list above, so what the type accepts is what V8's format carries.
Nothing on the host walks a value to check it — the serializer is the single
gate, and a value it refuses fails at encode time with its own data-clone
error.

### 5.2 Errors

Every run resolves to one of three outcomes, discriminated by `status`:
`'completed' | 'failed' | 'aborted'`. `ok` is kept as a convenience alias for
`status === 'completed'`, so the common guard stays `if (result.ok)`; reach for
`switch (result.status)` when a deliberate abort must be told apart from a
genuine failure.

A completed run:

```ts
{ status: "completed", ok: true, exports: { … }, stdout, stderr, durationMs, cpuTimeMs, bridgeCalls }
```

All three outcomes carry the run's timings (`durationMs` wall, `cpuTimeMs`
active execution) and `bridgeCalls` — per-attempt metadata recorded inside
the Rust runtime, including attempts blocked by limits (`blocked: true`); see
§5 above. Aborted runs carry these too: graceful termination (§14.7) has the
runtime send a real result frame on abort, so timings and `bridgeCalls` reflect
work done up to the abort. They report zeros and an empty `bridgeCalls` only
when graceful termination falls back to socket teardown (a CPU-bound run not
reading frames, or a signal already aborted at entry).

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
  cpuTimeMs: 3.7,
  bridgeCalls: [ … ],
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
  cpuTimeMs,    // real, from Rust's graceful abort result (0 on teardown fallback)
  bridgeCalls,  // records up to the abort (empty on teardown fallback)
}
```

The result is _always_ an object; `status` (or the `ok` alias) discriminates.
`run()` does not throw for sandboxed failures — only for infrastructure failures
(e.g., the Rust process crashed).

### 5.3 Host → sandbox calls (#58)

Bridge calls run sandbox → host. The call API is the other direction: invoke a
function that already lives inside the sandbox, with real typed arguments, and
get its return value back — what `export default { fetch(request) }` needs to
be a first-class shape.

```ts
// direct run — the module just evaluated, so its namespace is right there
await sandbox.run({
  code: `export default { async fetch(request) { return new Response('hi') } }`,
  call: { export: 'default.fetch', args: [request] },
})

// prepared prefix — the common case, nothing compiled per request
const prefix = await sandbox.prepare({ code: bundle })
await prefix.call({ export: 'default.fetch', args: [request] })

// rebinding globals per call — orthogonal, comes free
await prefix.call({ export: 'default.fetch', args: [request],
                    globals: { fetch: perRequestFetch } })
```

The decided semantics (issue #58):

- **Addressing is always relative to the module's exports**, never
  `globalThis`: a top-level exported function (`"handler"`) or a method on an
  exported object (`"default.fetch"`). For `run({ code, call })` the path
  resolves against the freshly evaluated module; for `prefix.call()` against
  the prefix module — live per run since prefixes re-evaluate (§11.6), so
  module-scope closure state is reachable.
- **`call` present ⇒ the result carries `value`** (the function's return
  value, awaited first when it is a Promise); **absent ⇒ `exports`. Never
  both.** Logs, `durationMs`, `cpuTimeMs`, and `bridgeCalls` are unchanged in
  either case; the wire's completion payload carries exactly one value blob
  either way.
- **The receiver is the object the final path segment was read from** —
  plain `a.b.c()` semantics (the namespace itself for a single segment), so
  handlers reading `this` work. A `globalThis` receiver would fail *silently*
  (`this.tag` → `undefined` in a 200 response); only the correct receiver or
  a loud error are acceptable, the same conclusion workerd reached
  (`ExportedHandler::self`). Path reads follow the prototype chain, so
  `export default new Worker()` resolves prototype methods. `export default
  class` does **not** work (its methods live on the prototype of *instances*)
  and fails cleanly with `ERR_CALL_TARGET_NOT_FOUND` — the code covering both
  "does not resolve" and "not callable".
- **Arguments cross as one V8 blob holding the array** (identity between
  arguments preserved — the `BridgeCall` convention), through the
  host-type-aware leg, so a real `Request` crosses in and a real `Response`
  crosses back. Args are host-authored (the trusted direction): no dedicated
  size limit, the frame read is capped by `memoryMb` exactly like bridge
  responses. The return value respects `maxExportBytes`.
- **A sync return value skips the poll loop entirely**; an async handler
  re-enters the same settle machinery as the module evaluation promise, so
  mid-request bridge calls work for free and poll rounds track bridge round
  trips, not `await`s. Path resolution and the call run inside the CPU
  budget, under the run's wall/CPU guards.
- A throw inside the handler is `ERR_USER_CODE`, exactly like postfix code.

This is a **capability, not a perf win** (recorded so it is not re-argued):
against the pull-over-the-bridge baseline a call frame saves ~2 % of a
request at every handler size measured. The warm-isolate roadmap (§13.2.1,
#64–#67) later makes the same API fast without changing its shape.

`sandbox.readExports({ code })` rounds out the deploy path: load a module
once and read its declaration exports (IaC-style limits/connections), with
handler exports skipped and reported. It is API surface, not protocol —
internally it is a plain run, which the export-skip rule (§5.1) already turns
into a declaration reader; a warm-isolate implementation can slot in behind
the same signature later.

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
| `0x04` | `Terminate`      | Ask Rust to abort a running run and reply with an `ERR_ABORTED` result (§14.7) |

**Rust → TS**

| Byte   | Name         | Purpose                                             |
| ------ | ------------ | --------------------------------------------------- |
| `0x01` | `BridgeCall` | Sandbox called `fetch` or a host-module function    |
| `0x02` | `StdioChunk` | Eager `console.*` output (stdout or stderr)         |
| `0x03` | `Result`     | Final result for a `Run` (always sent exactly once) |
| `0x04` | `Log`        | Internal runtime diagnostics                        |

### 6.3 Payload encoding

Structured data (arguments, results, exports) uses V8 serialization blobs
(`docs/protocol.md` §4). Raw bytes for stdio chunks. Plain UTF-8 for log
strings.

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

1. **No `node:*` builtins** (one exception). If a user does
   `import fs from "node:fs"`, it throws `ModuleNotFound` unless the host
   explicitly provided it via `imports`. The host can ship a curated
   `node:fs` if they want; the runtime won't. The sole runtime-provided
   `node:*` module is `node:async_hooks`, which exposes a minimal
   `AsyncLocalStorage` (run/postfix code only). See §16. A host-declared
   import of the same specifier takes precedence over the built-in.

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
| 2 ✅   | Precompile + `PrecompiledPrefix.run()` (originally via V8 startup snapshots; mechanism replaced by per-run prefix evaluation — §11.6)                                                                           | The canonical AI-agent prefix/postfix loop works                           |
| 3 ✅   | CPU budget enter/leave bracketing (async time exclusion)                                                                                  | Tight loops killed quickly; `await fetch` doesn't burn budget              |
| 4 ✅   | Generic host-bridge dispatch for globals (string / function / shimmed); `fetch` is just one allowed name on this path                     | Hosts can expose any allowlisted global; `fetch` works as a regular global |
| 5 ✅   | `@iso4/fetch` package: `createSafeFetch` with allowlist, DNS pin, private-IP blocking, no-auto-redirect                                   | Hardened default users can opt into in two lines                           |
| 6 ✅   | Imports: source modules (Flavor B); host-supplied ESM strings compiled per-isolate. No separate code-cache LRU — the stored prefix is the cache.   | `import { add } from "lib:math"` works when the host declares the source     |
| 7 ✅   | Imports: host modules — host provides a JS object; the shape crosses the wire as plain data and the Rust runtime builds the module natively (data leaves via the value codec, function leaves as trampolines dispatching `BridgeCall { targetKind: 1 }` with runtime-resolved names). Nested mixed objects supported via recursive walker. See §4.3. | `import { search } from "host:tools"` works for arbitrarily-nested mixed data/function shapes; bridge records report `host:tools.search` with no client-side name resolution |
| 8     | Custom `ArrayBuffer` allocator, near-heap-limit graceful kill, hard wall-clock guard separate from CPU budget                             | Memory and time limits are tight under adversarial input                   |
| 9     | Pre-warmed isolate pool (optional, behind a runtime option)                                                                               | Sub-2ms cold start for high-throughput workloads                           |
| 10    | Polish: error types, integration tests, READMEs, examples                                                                                 | Shippable v1                                                               |
| 11    | **Sync bridge calls.** Per-leaf sync tag on the wire shape; the runtime builds a sync trampoline (`__iso4_call_sync`, blocks the isolate on UDS read) instead of the async one. Leaf classification + the opt-in-vs-auto policy question are covered in the §4.3 "sync function leaves" note. CPU budget bracketed; wall guard preempts via `terminate_execution`. | `import { readFileSync } from "host:fs"` works without `await`. Foundational for a future node-compat layer. |
| 12    | ~~Native host-module binding.~~ Superseded by #37: host modules already cross as data and are built natively by the runtime, which owns the handle table and emits `BridgeCall { targetKind: 1 }` with resolved names. Full synthetic modules remain deliberately unused (native evaluation-steps pointers would need external-reference bookkeeping to survive prefix snapshots). | — |
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

- The wire-frame types (value blobs, the bridge payload conventions).
- The "only data crosses" rule and the blob-based value contract.
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

- **Prefix store LRU cap.** Prepared prefixes (source + declared shape)
  live in the Rust process's memory, typically a few KB each. Default cap:
  100 prefixes, LRU evicted.
  Configurable via `SandboxOptions.maxPrecompiledPrefixes`. Evicted handles
  fail `.run()` with `ERR_PREFIX_DISPOSED`; the host re-precompiles.

- **Mandate `createSafeFetch` or recommend it?** The core `iso4` package's
  `fetch` field accepts any `FetchHandler`. The library does mechanical
  hygiene (header/URL validation) at the bridge; the host author decides
  policy (allow/deny). `@iso4/fetch` ships hardened defaults but is opt-in.
  Documented strongly.

- **`Session.call()` input/output serialization contract.** For the
  persistent-session API of the analytics product, the host calls a function that was
  exported by the prefix. What can be passed as `input`? A value blob is the
  natural choice (same as exports today), but bytes could be transferred
  zero-copy if needed for bulk data. Decide when designing the analytics product (§9.1).

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

`Runtime.precompile()` validates the prefix once (compile + instantiate +
evaluate in a throwaway isolate) and stores its source and declared shape in
the Rust process. `prefix.run()` boots a fresh isolate from V8's stock
snapshot, re-evaluates the prefix, then runs the postfix. The expensive,
error-prone part of the loop — authoring and validating the setup — is paid
once; runs pay only the evaluation.

Flow:

```
  precompile(prefix)            run(postfix)          run(postfix)
       │                            │                      │
       ▼                            ▼                      ▼
  one-time:                    per run:               per run:
  - validate in throwaway      - fresh isolate        - same
    isolate (compile +           (stock snapshot,
    instantiate + evaluate)      ~0.3 ms)
  - cache source +             - web runtime install
    declared shape             - evaluate prefix
                               - bind per-run globals
                                 & imports
                               - compile + run postfix
```

Steady-state cold start target (after the first call): **<5 ms** from
`prefix.run()` to user code executing, for typical (small) prefixes. Run
cost grows with prefix size — a prefix that takes 20 ms to evaluate costs
every run 20 ms. Heavy prefixes are the domain of the resident-isolate
model (§11.6, epic #61).

### 11.3 The prefix contract

Each run re-evaluates the prefix into a fresh context, so what a postfix
sees is exactly what the prefix produces deterministically:

- **Provided by prefix evaluation (identical across runs, assuming the
  prefix is deterministic):**
  - All `globalThis` state the prefix sets up.
  - Declared value globals (string/data kinds) and shim wrappers, replayed
    from the stored defs before the prefix evaluates.
  - Source-module and host-module imports, rebuilt from the declared shape.
- **Bound per run:**
  - Bridge function stubs. While the prefix evaluates they are throwing
    placeholders; before the postfix runs the runtime overwrites the same
    names with live stubs bound to the run's socket and handlers.
- **Constraints (identical to the snapshot era, by design):**
  - The prefix's evaluation promise must settle without host I/O. Top-level
    `await` is fine as long as nothing external is awaited: the runtime
    drains the microtask queue after evaluation (`await 1`,
    `await Promise.resolve(x)`, `await new Response(body).text()`, chained
    awaits all settle). A prefix that can never settle (e.g.
    `await new Promise(() => {})`) fails with `ERR_PREFIX_DID_NOT_SETTLE`;
    the checkpoint loop is bounded, so nothing hangs.
  - Prefix code cannot *call* the bridge. The bridge does not exist while a
    prefix evaluates; every declared bridge callable — bridge globals, shim
    globals, host-import functions — is a throwing placeholder at that
    stage: it exists (`typeof fetch` matches what run() code sees) and may
    be referenced, stashed, or closed over, but calling it fails with
    `ERR_PREFIX_BRIDGE_CALL`. Whether prefix-stage bridge calls ever become
    supported is a deliberate future decision; the error code is the gate.
  - A nondeterministic prefix (`Math.random()`, `Date.now()`) produces
    per-run state that differs between runs — validated once, evaluated
    many times. This was impossible in the snapshot era and is permitted
    but not encouraged; determinism keeps runs reproducible.
  - Prefix evaluation runs under the run's wall/CPU limits. A prefix that
    loops costs the run its budget instead of hanging `prepare()`.

### 11.4 Rebinding rules

When `prefix.run()` provides `globals` or `imports`, the replacement
handlers stay on the TS side — bridge dispatch is name-addressed, so a
rebind just re-points the per-run dispatch entry (global name, or
host-import `specifier` + leaf path). The Rust runtime validates every
rebound name/location against the shape stored with the prefix. Source
modules cannot be rebound — their code is frozen at declaration.

If `prefix.run()` passes a name that wasn't declared at `precompile()`
time, the run fails fast with `ERR_UNDECLARED_BINDING`. This is intentional:
we could silently install new globals into the run's context, but that
breaks the invariant that the declared prefix shape represents the full
sandbox surface the prefix was validated against. Better to be strict and
force the user to declare their surface up front.

If the prefix declared a global the run doesn't supply, the precompile-time
implementation is reused. This makes it easy to provide a default at
precompile time and override only when needed per run.

### 11.5 Pool of pre-warmed isolates (phase 9)

Isolate boot + prefix evaluation costs low single-digit ms for typical
prefixes. For high-throughput workloads we keep a pool of N already-warm
isolates per prepared prefix, sitting idle until a run grabs one. Pool
size, eviction policy, and warmup-on-prepare are all `SandboxOptions`.

Not in v1. Per-run evaluation keeps steady-state cold start under 5 ms for
typical prefixes, which is plenty for interactive agents. Warm isolates are
the answer for sub-millisecond requirements and heavy prefixes — tracked as
epic #61 (registry + taint-and-evict + capacity manager + eviction scoring).

### 11.6 Decision record: why there is no runtime snapshotting

Until 2026-08 the prefix mechanism was a **V8 startup snapshot** created at
`prepare()` time (`SnapshotCreator` + `create_blob`) and restored per run.
It was removed deliberately (#60 → #61/#62); do not reintroduce it without
reading this.

**What broke.** V8 14.x made two changes that are fatal to runtime snapshot
creation in a live multi-isolate process:

1. `create_blob` runs *read-only promotion*: it mutates and re-seals the
   read-only heap that is **shared process-wide across all isolates**. Two
   concurrent `prepare()` calls race on it and segfault the child
   (SIGSEGV/SIGBUS inside `ReadOnlyPromotion`/`ReadOnlySpace`).
2. `IsolateGroup::RemoveIsolate` **frees the shared read-only artifacts when
   the last live isolate dies** and rebuilds them on the next boot.
   Snapshots created before such a reset reference read-only pages that no
   longer exist afterwards (`Deserializer::ReadReadOnlyHeapRef` indexes out
   of bounds). iso4 constantly crosses the zero-live-isolates boundary
   between runs, and **no locking can fix this one** — it is state loss,
   not a race. Verified empirically: fully serializing every isolate boot
   and `create_blob` still crashed 9/10 test runs.

A working mitigation existed (a global mutex for (1) plus an immortal
"keeper" isolate pinning the artifacts for (2) — 10/10 green under stress),
but it papered over an unsupported usage rather than fixing a bug on a
supported path.

**Upstream position.** V8's snapshot owner describes the supported workflow
as mksnapshot's: "a single snapshot of a well-known, simple heap state,
then throw the Isolate away" — anything beyond it will "soon run into
trouble", and multiple custom snapshots "have divergent read-only heaps,
violating an invariant" (v8-dev, threads on custom-snapshot checksum
failures and SnapshotCreator teardown). The field agrees: workerd and Deno
snapshot only at build time; Node's `--build-snapshot` is a one-shot
single-isolate process; isolated-vm shipped runtime snapshots and now warns
"you should not use this feature … increasingly unstable due to changes in
v8".

**Re-entry path.** V8 14.x introduced `IsolateGroup` internally — separate
groups own separate read-only heaps, which would legitimize per-snapshot
groups. If that (or an equivalent) becomes public embedder API and rusty_v8
exposes it, snapshots can return behind the unchanged `prepare()` API as a
pure optimization. Until then: prefix = validated source, re-evaluated per
run; heavy prefixes and sub-ms calls belong to the warm-isolate model
(epic #61).

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
they terminate V8 execution immediately via `terminate_execution()` — the
violation is uncatchable, no sandbox code runs after it, and the run fails
with the corresponding error code.

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

`@iso4/sandbox` ships one call shape — **independent runs**:

```
prefix.execute({ code }) / prefix.call({ export, args }) → RunResult / CallResult
```

Each run executes against the prefix and returns its result in one IPC round
trip (Run → … → Result). One-off `sandbox.run()` creates a fresh isolate per
run and tears it down. Prefix runs are served by **warm instances** (#64,
§13.2.1): resident isolates that skip boot and prefix re-evaluation on
reuse — a transparent cache, not a semantic change; state carryover between
runs is permitted but never guaranteed. This is the AI-agent prefix/postfix
pattern described in §11 and the request-handler pattern from §5.3.

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

The isolate is created once (prefix evaluated once) and kept alive for the
lifetime of the session. The prefix exports a function (`transformRow` above)
that the host calls repeatedly with different inputs. No new isolate cost
per call; no prefix re-evaluation per call.

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
│──── OpenSession ─────────────────────▶│  boot isolate, evaluate prefix
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

Opening a session cold (isolate boot + prefix evaluation + thread spawn)
costs low single-digit ms.
For a workload that receives bursts of parallel requests, that latency
stacks. The session pool solves this: the runtime pre-warms N isolates
from the prepared prefix and holds them idle. `prefix.openSession()`
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
- `session.close()` returns the isolate to the pool (reset to a fresh
  prefix state) rather than destroying it, so it's ready for the next
  request.
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

### 13.2.1 Warm instances (#64 — shipped)

Every prepared prefix is served by **warm instances**: resident isolates
with the prefix already evaluated. The first `execute()`/`call()` on a
prefix cold-starts an instance; later runs reuse it, skipping isolate boot
(~190 µs), context creation (~115 µs), the runtime installs, prefix
evaluation (scales with prefix size), and teardown (~180 µs). This is
automatic — no flag, no separate API, no wire change. The host decides
nothing; the runtime reuses or cold-starts transparently per `PrefixRun`.

**Contract: warmth is a cache, never a guarantee.** State carryover between
calls on one instance is permitted but may vanish at any moment (taint,
scored eviction, dispose) — the workerd stance. Relying on carryover is an
antipattern; relying on per-run isolation within one prefix is simply wrong.
Durable state belongs in the DB; instance memory is only an optimization.
The supported pattern for expensive setup is **lazy init inside the
handler** (`conn ??= await setup()` on first call): it runs under that
call's budget, through that call's bridge, and re-runs correctly after any
eviction.

**Threading.** rusty_v8 pins isolates to their creating thread
(`OwnedIsolate` is `!Send`, `Drop` asserts current-thread ownership and
reverse creation order, no `Locker` exposed — re-verified on v8 147.4.0).
Each instance is therefore owned cradle-to-grave by a dedicated runtime
thread; session threads forward calls over a channel and park on the
response, so the session socket keeps exactly one user at a time (the
instance thread does bridge I/O during a call). Idle instances hold memory,
not threads-in-use, and no clocks are armed while idle.

**Warm-up budget.** Prefix evaluation (plus the per-instance runtime
installs) runs under a fixed 1 s wall / 1 s CPU budget — isolate boot itself
(~0.2 ms, not sandbox-controllable) precedes it — never the triggering request's limits
(Cloudflare's separate script-startup limit is the model; theirs is 1 s
since 2025-10 and likewise not configurable). Blowing it reports
`ERR_WARMUP_LIMIT`. `prepare()` enforces the same budget, so an un-warmable
prefix fails at deploy time, not on every cold start. (Bonus: a prefix with
a synchronous infinite loop used to hang `prepare()` forever.) Prefix
evaluation stays bridge-less: `ERR_PREFIX_BRIDGE_CALL` /
`ERR_PREFIX_DID_NOT_SETTLE` fire at warm-up exactly as at `prepare()`.

**Per-call semantics on a warm instance** are identical to a one-off run:
fresh wall/CPU guards and a fresh CPU meter from dispatch to settle, bridge
stubs re-installed per call (throwing placeholders re-armed first so a stub
the next call does not re-bind can never dangle), per-call console caps.
The heap cap is the exception — see below.

**Taint-and-evict.** Any fired guard (CPU, wall, heap), an abort landing
mid-call, a fatal bridge violation, or an internal failure discards the
instance: `terminate_execution` rips arbitrary mid-execution state, so
prefix coherence is unprovable afterwards. Ordinary uncaught exceptions are
clean completions and do NOT taint. The next call pays a cold start; the
misbehaving tenant pays, everyone else is unaffected. Never reuse a tainted
instance; never evict a running one.

**Uniform heap cap.** `memoryMb` moved from per-run `ResourceLimits` to
`createSandbox()` (default 128 MB, workerd's number): the cap is baked into
`Isolate::new` and instances are reused, so a per-run value is structurally
impossible, and a uniform cap keeps capacity math `slots × cap`. Heap and
ArrayBuffer usage accumulate across calls on an instance — hitting the cap
taints it. Passing the old per-run field throws.

**Capacity (v3, #66): one RSS mark, scored eviction — celld's model,
whole.** Two independent resources, two knobs. `maxIsolates` (the
connection pool size) caps **concurrent runs**; the **memory budget**
(`memoryBudgetMb` on `createSandbox`, passed as `--warm-budget-bytes`,
`0` = disabled) is the ONE memory mark, enforced by the runtime watching
its **own process RSS** — ground truth, where summed heap numbers
undercount (external ArrayBuffers, V8 overhead, allocator fragmentation,
later SQLite), and the number the container OOM killer actually acts on.
Sampled per registry event (~0.4 µs `task_info` / `statm` read — no
polling timers), folded through pure decision functions (`policy.rs`, the
same replaceable-rule style #77 will use):

- **RSS at/above the budget latches shedding**: evict idle instances by
  `heapUsed × idleTime` score — highest first, ties to the longest-idle —
  a tenth of the idle population per pass, AND stop pooling NEW instances:
  a `PrefixRun` without an idle instance runs on a cold one-off isolate
  (fresh per call, never pooled, one-off accounting); reuse of
  already-warm instances stays allowed since it adds no memory (celld: a
  pressured node "may keep serving on the isolates it already has, but
  must not build another"). Correctness never depends on warmth; no
  error, no new error code. **No grace period** after last use: the
  idleTime factor already sends a just-used instance to the back of every
  pass (celld sheds in plain LRU order for the same reason; recorded
  on #66).
- **The latch releases at 4/5 of the budget** — the hysteresis gap that
  stops evict/admit flapping (celld's ratio).
- **Futility check**: freed heap returns to the OS lazily, so a pass that
  left RSS flat (within 5 %) stops the walk instead of evicting the world;
  the latch holds, and a sample that moves either way re-arms it.

There is deliberately **no instance-count cap** (the #65
`--max-live-isolates` is gone): celld defaults its resident ceiling to
unlimited after a default count cap caused eviction churn, and a count
answers a question ("how many?") that memory pressure — the thing that
actually kills the process — cannot be read from. Concurrency is bounded
by the host pool, memory by the mark; running instances are never
evicted. The budget default is container-aware:
`process.constrainedMemory()` (falling back to `os.totalmem()`, which lies
in containers) minus a safety net of max(512 MB, 25 %) for the Node host,
the Rust runtime, and the embedding service's own per-isolate state — the
mark then compares the Rust process's own RSS against its own budget, so
no extra leniency is needed (celld's default ceiling of 80 % of total
plays the same headroom role). Independent of `memoryMb`: RSS is
measured, not derived from per-isolate caps. One-off runs never touch the
warm pools but share the same ledger and pressure checks; they are never
refused — transient work gives its memory back on its own.

**Saturation and stats (#65).** Saturated run slots always queue FIFO —
deliberately no per-call policy knobs (fail-fast / max-wait were built and
removed: every run has wall/CPU limits, so the wait is bounded by the
running calls themselves, and the knobs only complicated the API). Smarter
admission is #77's cost model. `sandbox.stats()` returns a point-in-time
snapshot (active runs, queue depth, warm/idle instance counts, summed idle
heap, the mark and the signal it acts on (`budgetBytes`/`rssBytes`) and
the shedding latch (`underPressure`, #66), per-prefix counts) over a
**dedicated control connection** outside the run pool, so it answers
precisely when everything is saturated. `used_heap_size` is reported on
every `PrefixRun` Result frame (`heapUsedBytes`) and feeds the
`heapUsed × idleTime` victim scoring. Per-prefix fairness caps and the
wait-vs-cold-start acquire policy are deliberately not here: they are
#77's cost model, built on the per-prefix busy/idle state the registry
now tracks.

**Instance pools, not singletons.** The registry maps prefix → pool of
instances, because the same trigger fires concurrently: a call takes an
idle instance or cold-starts another. Instances of one prefix share **no
state** with each other (same contract as workerd instances across
machines). **v1 concurrency: one call at a time per instance** —
parallelism for one prefix = more instances. Async interleaving of multiple
in-flight calls inside one isolate (the full workerd model) stays deferred:
it needs multiplexed bridge calls and per-request context separation.

**One-off runs are untouched**: `sandbox.run()` always gets a fresh isolate
with unchanged semantics.

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

Bridge return values are plain data (a value blob). Functions on returned
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

Rationale: the declared shape is what the prefix was validated against.
Silently installing an undeclared global into a run's context would widen
that surface in a way the prefix never saw.

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
`ERR_FUNCTION_ARGUMENT_NOT_SUPPORTED`. Only serialisable data values may
cross the boundary in either direction in v1.

This is a deliberate limitation — callbacks across the boundary would require
a callback-handle protocol symmetric to callable return values. The same Phase
11 work that adds callable return values will enable function arguments too.

### 14.7 In-flight run abort via `AbortSignal`

`run({ signal })` and `prefix.run({ signal })` honor an `AbortSignal` that
fires **at any point during a run**, not just at entry. A pre-aborted signal
short-circuits to `ERR_ABORTED` before any frame is sent; an abort that lands
mid-run stops the run promptly and, wherever possible, **gracefully** — with a
real result frame from Rust rather than a synthesized one (#36).

Graceful mechanism (the common case — a run suspended awaiting a bridge
response, which is exactly how `durable-isolates` suspension works):

1. **Send `Terminate`**: `drainUntilResult` subscribes to the signal. On abort
   it writes a `Terminate` frame (carrying the `runId`) and keeps draining,
   leaving the socket open.
2. **Rust aborts and reports**: the run's poll loop is parked on the socket read
   awaiting a `BridgeResponse`; it reads the `Terminate` instead, calls
   `terminate_execution()`, and returns an `ERR_ABORTED` result carrying the
   real `durationMs`, `cpuTimeMs`, and the bridge-call records collected so far.
3. **Remap and reuse**: that result flows back through `drainFrames`; `index.ts`
   sees the aborted signal + `ERR_ABORTED` code and remaps it to
   `status: 'aborted'` with the abort `reason`, keeping the telemetry. The
   connection stays healthy and is **returned to the pool** — no reconnect.
4. **Drop the late response**: an orphaned bridge handler that resolves *after*
   the abort writes its `BridgeResponse` onto the (reused) connection, where it
   is discarded — session.rs ignores stray responses and the monotonic
   per-connection call-ID counter guarantees a stale callId never matches a
   later run's resolver. The sandbox therefore never observes a return value for
   the call that was in flight when the abort landed, so `controller.abort()`
   from inside a bridge handler is a spoof-proof way to stop a run.

**Fallback (CPU-bound caveat)**: a purely CPU-bound run (no bridge call in
flight) is spinning inside `module.evaluate()` and never reaches the poll-loop
frame read, so Rust cannot consume the `Terminate`. If no result arrives within
a short grace window (`TERMINATE_GRACE_MS`, ~100 ms), TS falls back to the
teardown path: it closes the frame reader and destroys the socket. The `run()`
promise resolves as aborted immediately with synthesized zeros, the connection
is marked unusable and replaced by `ConnectionPool` (keeping the full
`maxIsolates` complement), and the abandoned isolate is reclaimed only when its
**CPU guard** fires — bounded by `cpuTimeMs`, not `wallTimeMs`. Promptly
interrupting a busy isolate would require a Rust-side `terminate_execution`
driven by an out-of-band signal (e.g. a socket-hangup watcher); this is
deferred until a consumer needs it.

The `run()` promise resolves with `status: 'aborted'` (see §5.2) in all cases —
carrying the value passed to `abort(reason)`, and retaining `error.code:
'ERR_ABORTED'` for backward compatibility.

---

## 15. Callable handles (Phase 13)

### 14.1 The problem

Bridge return values are plain data (a value blob). This means a host handler
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
a plain-data marker `{ __iso4_fn__: <id> }` before serializing the blob.
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

The Rust side detects `{ __iso4_fn__: <id> }` objects after deserializing and
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
  ValueBlob        args   (one blob holding the argument array)
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

The result of a callable method call is itself serialized as a value blob.
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

---

## 16. Async context (`AsyncLocalStorage`)

Sandboxed code can carry an ambient value across `await` points — a request
or trace id, a durable-workflow step key, "what context am I running in" —
without threading it through every function signature, and without concurrent
async chains mis-attributing each other's context.

### 16.1 Surface

A minimal, Node-compatible subset of `AsyncLocalStorage`, imported the same way
as in Node:

```js
import { AsyncLocalStorage } from 'node:async_hooks'

const als = new AsyncLocalStorage()
als.run(store, callback, ...args) // run callback with `store` as the current
                                  // value; auto-restores on scope exit
als.getStore()                    // read the current value (or undefined)
```

Only `run` and `getStore` are implemented — the concurrency-safe core. `run`
already takes a callback, so it scopes cleanly and composes for nested use.
`enterWith`/`exit`/`disable`/`snapshot` are intentionally omitted (`enterWith`
in particular is unsafe under concurrent branches and unnecessary when a
callback boundary exists). They can be added later if a concrete need appears.

The canonical use case is a durable-workflow `step.do(name, fn)` shim: each
nested step appends a segment to a key held in an `AsyncLocalStorage`, so a
step nested inside another produces `parent/child` and the same step name used
elsewhere never collides.

### 16.2 Mechanism

Built on V8's **continuation-preserved embedder data** (CPED) — the same
primitive modern Node's `AsyncContextFrame` uses, exposed by the `v8` crate as
`Context::{Get,Set}ContinuationPreservedEmbedderData`. V8 automatically saves
the CPED slot with each promise continuation and restores it when that
continuation runs, so the ambient value follows `async`/`await` chains and
concurrent chains stay isolated.

The runtime installs two native functions (get/set the CPED slot) and hands
them to a small JS factory that returns the `AsyncLocalStorage` class closing
over them. The class is stashed on `globalThis` under `Symbol.for(...)`; the
built-in `node:async_hooks` module re-exports it. The native get/set are never
exposed to user code — they live only in the factory closure. Contexts are
singly-linked frames (`{ parent, instance, value }`) carried in the CPED slot,
built from object literals and read by own-property access, so user code
tampering with builtin prototypes cannot subvert propagation.

**No promise hooks are registered.** A run that never constructs an
`AsyncLocalStorage` pays nothing beyond V8's own (cheap) CPED bookkeeping. The
per-`await` cost of using it is small in-sandbox JS time, billed to
`cpuTimeMs`/`wallTimeMs` like any other work — never to the bridge-call budget.

### 16.3 Interaction with the rest of the model

- **Always available at run time, no opt-in flag.** Because it registers no
  hooks, it can be always-on like `console`; matches Node's "importable
  always, cheap unless used" behavior.
- **Per-run isolation.** State lives in the per-run isolate's CPED slot and
  resets between runs with everything else.
- **Only `await`-based async is covered.** CPED propagates across promise
  continuations, not timers/callbacks — and the sandbox has neither
  (limitations §7.2, §7.8), so this covers the entire async surface.
- **Not available in prefix/precompile code.** `node:async_hooks` does not
  resolve while a prefix evaluates (at `precompile()` validation or at a
  run's prefix stage); it fails cleanly with `ERR_MODULE_NOT_FOUND` — a
  contract kept from the snapshot era so prefixes behave identically. Setup
  code is the prefix; async context is for the postfix (agent/workflow code).
  The durable-workflow pattern is to keep the `step`/`AsyncLocalStorage` shim
  in a **run-time import** the postfix pulls in (resolved per run, where
  `node:async_hooks` is available), while the expensive tool/data setup lives
  in the prefix. See §7.1.

### 16.4 Precompile validates ahead of runs

`precompile()` compiles, instantiates, and evaluates the prefix in a
throwaway regular isolate, in exactly the environment a run's prefix stage
provides. Any bad prefix (syntax error, unresolved import such as
`node:async_hooks`, throwing top-level code) returns a clean error at
`prepare()` time instead of failing every subsequent run. Prefix code is
host-authored setup with no bridge/network side effects, so evaluating it at
validation and again on every run is safe.
