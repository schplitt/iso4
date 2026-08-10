# RFC 0001 — Calling into the sandbox

**Status:** options prepared, nothing decided. Every decision below is the
maintainer's call.

**Scope:** design only. This RFC adds no code, no frames, and no protocol
changes. It exists so that seven open decisions can be picked up cold, with the
constraints, the costs, and a recommendation for each.

**Companion decision:** the SQLite access path (§6). It should be made *before*
the call API, not after, because it is the one that can move an architectural
rule.

---

## 1. The gap

iso4 has no way to call a function that already lives inside the sandbox.
Every call today runs the other way:

- **`BridgeCall` / `__iso4_call`** — sandbox → host. Arguments already cross as
  one V8 blob. This is the mature direction.
- **`Run` / `PrefixRun`** — "compile this module, evaluate it, give me
  everything it exported." That is whole-module evaluation, not a call. There is
  **no argument channel**: the only ways in are a `data` global (a snapshot
  constant on the prefix path — see below) or interpolating the value into the
  postfix source.

What is missing:

```
call(exportPath, argsBlob) -> resultBlob
```

Resolve a function that already lives in the isolate, call it with deserialized
arguments, send the return value back as a blob. Both **exported functions** and
**methods on exported objects** need to be callable, because the target product
is hosting request/response web functions — the `export default { fetch }` shape.

### 1.1 Why `data` globals do not close the gap

`GlobalDef` kind `0x02` (`data`) materialises a value onto `globalThis`
(`docs/protocol.md` §5.2). On the **`Run`** path that is a real argument channel.
On the **`PrefixRun`** path it is not: string/data globals and shim wrappers are
baked into the snapshot at `Precompile` time, and a `PrefixRun` may only carry
`bridge`-kind entries. Data globals on a prefix are therefore **snapshot
constants**. There is currently no way to put a handler in a snapshot and feed it
a fresh `Request`.

`ImportRebind` does not help either: a `PrefixRun` may rebind only the
*locations* of host-module **function** leaves, never a data leaf
(`docs/protocol.md` §5.2, enforced with `ERR_UNDECLARED_BINDING`).

So today exactly two per-request argument channels exist on a prepared prefix:

1. **Interpolate** the value into the postfix source string.
2. **Pull** it back over the bridge — a fixed postfix calls a host global that
   returns the request.

Both work. §5 measures them. Option 2 turns out to be the faster of the two and
needs no protocol change at all, which materially changes how urgent a `call`
frame is.

---

## 2. Restated context

`internal/API_DECISIONS.md` and `internal/OPTIMIZATIONS.md` are gitignored and
exist on one machine. Everything this RFC depends on is restated here so a reader
with only the public repository can follow it.

### 2.1 Established by an earlier spike

- **Module namespaces do not survive the snapshot.** `precompile_module` blobs
  the *context*; after restore the module object is gone and only what the prefix
  put on `globalThis` comes back. An `exportPath` therefore cannot resolve
  against a prefix's module namespace.
- **Functions stashed in a plain object on `globalThis` do survive**
  `create_blob(Keep)` and remain callable after restore.
  `packages/iso4-sandbox/tests/integration.test.ts` already relies on this
  behaviour (the "precompile stress" prefixes publish lookup tables on
  `globalThis`).
- `Function::call` on an already-materialised argument is **39 ns**; a full
  blob-in → call → blob-out cycle is **606 ns** — against a per-run isolate cost
  three orders of magnitude larger. This is why the mechanism was originally
  filed as a performance item and deferred at **0.5 %** of a one-shot run. That
  verdict is correct for performance and answers a different question than this
  RFC: as an **API capability** the same mechanism is the prerequisite for
  `export default { fetch(request) }`, for calling named tenant handlers, and for
  passing real arguments in at all.

### 2.2 Established by the run-tax decomposition

Hot `prefix.execute()`, sparse payload, release binary, idle machine, p50:

| phase | µs | share of a 609 µs run |
| --- | ---: | ---: |
| `Isolate::new` (incl. snapshot deserialize) | 191.7 | 31 % |
| `Context::new` | 115.9 | 19 % |
| `Isolate::Dispose` + scope teardown | 179.8 | 30 % |
| **the three together** | **487.4** | **80 %** |
| everything else in Rust (installs, guards, compile, both codec legs) | 86.7 | 14 % |
| Node + socket | 34.7 | 6 % |

Two consequences that matter here:

- Replacing compile+instantiate+evaluate with a `Function::call` moves ~8.5 µs.
  Skipping `install_console` (2.4), `install_async_context` (15.7) and bridge
  stubs (2.1) would move ~20 µs more — but that is a **different execution
  contract**, not a transparent optimisation: without them `console.*` throws and
  `node:async_hooks` does not resolve. A request-handling product needs both.
- **Snapshot *content* is a per-run tax, not a one-time cost.** A prefix that
  bakes 4 MB of data into globals produces an 8.75 MB blob and costs 8.8 ms in
  `Isolate::new` on *every* run. §5.6 shows the same effect for handler *code*.

### 2.3 Established by PR #52 (merged)

- `Request` / `Response` / `Headers` cross the boundary as real instances in
  **both** directions, at any depth (`docs/protocol.md` §4.4). The argument
  channel can carry a real `Request` from day one — verified end-to-end in §5.4.
- The sandbox ships a web runtime (`Headers`, `Request`, `Response`,
  `TextEncoder`, `TextDecoder`, `URL`, `URLSearchParams`) captured in the prefix
  snapshot, and a working `ExternalReferences` table so native-backed classes
  survive `create_blob`. That table is the mechanism any future native global
  (§6) would also need, and it is now proven.

### 2.4 Release window

`PROTOCOL_VERSION` is **2** in `native/v8-runtime/src/ipc.rs`, and protocol v2 is
**unreleased**: the published `@iso4/sandbox` is 0.4.1 (protocol v1) and the
changesets release PR is still open. Until that release lands, v2 frame layouts
can be **extended in place** without a version bump. After it lands, an additive
field on an existing frame is a v3 change. This is a real, closing window and it
bears on §4.6.

---

## 3. Constraints the answer has to live inside

From `AGENTS.md` and `DESIGN.md`, in the order they bite:

1. **Only data crosses the boundary.** Functions never cross by value. A call API
   moves *arguments and return values*, never callables.
2. **No callbacks across the boundary in v1.** The called function may not
   receive a host function as an argument (`ERR_FUNCTION_ARGUMENT_NOT_SUPPORTED`).
3. **Two-process is the only backend.** The Rust subprocess provides crash
   isolation for untrusted code. There is no `backend` flag and no in-process
   NAPI embedding in `@iso4/sandbox`.
4. **One execution model in this product.** `@iso4/sandbox` ships one API shape:
   one-shot runs. A persistent-session product is a separate product
   (`DESIGN.md` §9.1, §13.1). This constraint is what §4.5 turns on.
5. **User code is always ESM; results come back via exports, never globals.**
   This one cuts against the "make the prefix publish its handler on
   `globalThis`" option in §4.1.
6. **Globals are runtime-curated**, with a reserved-name list enforced by
   `ERR_UNDECLARED_BINDING`. Any auto-stash slot joins that list.
7. **No new top-level globals may be installed into a restored snapshot at run
   time** — it breaks the snapshot's shape invariant. Installing at *snapshot*
   time is fine.

---

## 4. The seven decisions

### 4.1 Where does the callable live?

**The decision.** How does a function the prefix defined become reachable by name
from outside the isolate?

**Constraint, measured for this RFC.** The stash slot must be a
JavaScript-visible property. Three candidates were planted in a snapshot and read
back after restore:

| slot | survives `create_blob(Keep)` | reachable from sandbox JS |
| --- | --- | --- |
| string key (`globalThis.__iso4_exports`) | ✅ callable after restore | ✅ `Object.getOwnPropertyNames` |
| `Symbol.for('iso4.exports')` | ✅ callable after restore | ✅ `Object.getOwnPropertySymbols` |
| `v8::Private` symbol | ❌ **absent after restore** | — |

`v8::Private` does not survive, for either `Private::new` or the registry-backed
`Private::for_api`. **There is no hidden slot.** Whatever is chosen is visible to
sandbox code and must be added to the reserved-name list.

**Options.**

- **A — auto-stash the prefix's module namespace at `prepare()`.** After the
  prefix module evaluates, `precompile_module` copies its namespace into a plain
  object and sets it on `globalThis` under a reserved name, before `create_blob`.
  `export default { fetch }` then just works, matching the Workers mental model
  and `DESIGN.md`'s "results come back via exports" rule. Cost: one object copy
  at snapshot time (the same work the run path already does for exports, ~1.8 µs
  on the measured phase table). One new reserved name.
- **B — require the prefix to assign `globalThis.handler = …` itself.** No
  reserved name, no runtime change to `precompile_module`, minimal magic. But it
  contradicts constraint 5: the product's stated idiom is that a module's surface
  is its exports, and this makes the sandbox's *entry point* the one thing that
  must be published as a global. It also gives no natural addressing scheme —
  every host invents its own.
- **C — auto-stash, filtered.** Copy only exports that are functions, or objects
  whose own enumerable properties include a function. Narrower surface; but the
  filter is a guess about intent, and it makes `prepare()` reject or silently
  drop shapes that look fine to the author.

**Recommendation: A.** It is the only option that keeps the ESM-exports idiom
intact, and the reserved name is a cost the runtime already knows how to charge
(`console`, `Response`, … are enforced the same way). Two details to decide with
it:

- Use a **string key** rather than `Symbol.for`. Both are equally visible, and a
  string key is enforceable by the existing reserved-name check, greppable, and
  shows up in the error message a colliding host global produces.
- Because the slot is writable by sandbox code, the runtime must resolve the path
  **before any sandbox code runs in that isolate**. That is automatic if a frame
  carries either a postfix *or* a call but never both — see §4.6.

*Worth noting:* the namespace copy captures export values as they were at
snapshot time. Correct for handlers; it does mean `export let counter`
reassignments after the snapshot are not observable. No handler shape depends on
that.

### 4.2 Addressing

**The decision.** A dotted string resolved per call, or an opaque handle
registered at `prepare()`?

**Measured** (warm isolate, p50):

| | µs per call |
| --- | ---: |
| resolve `"__iso4_exports.default.fetch"`-shaped 2-segment path | 0.250 |
| materialise a cached `Global<Function>` handle | 0.041 |

The difference is **0.2 µs**, against a ~29 µs bridge crossing and a ~1,100 µs
one-shot isolate: 0.02 % of a request.

**Options.**

- **A — dotted string, resolved per call.** No registry, no handle lifecycle, no
  second API call, and the wire payload is self-describing (a `Run` frame you can
  read tells you what it called). Costs 0.25 µs and introduces a name-resolution
  surface.
- **B — opaque handle registered at `prepare()`.** Saves 0.2 µs, avoids
  re-resolution, and removes the name-resolution surface. Costs a per-prefix
  handle table, a handle lifecycle tied to prefix disposal, an extra round trip
  at prepare time, and a second failure mode (stale handle after
  `ERR_PREFIX_DISPOSED`).

**Recommendation: A.** The performance argument for B is inside the noise, and it
buys real lifecycle complexity. Two constraints to attach:

- **Cap the segment count** (4 is generous) and validate each segment as a plain
  property key. This bounds the resolution surface.
- **Resolve after `cpu_budget.enter()`.** A path segment can be an accessor or a
  proxy trap; the prefix is host-authored so this is not an attack, but running
  arbitrary sandbox code *outside* the CPU-budget bracket would silently
  mis-account it. Alternatively use own-property-only lookups, which also
  prevents a path from walking the prototype chain.

### 4.3 Receiver (`this`)

**The decision.** `Function::call` needs a receiver. What is it for
`"default.fetch"`? This is what makes **methods** callable at all, which is half
of what the product needs.

**Measured.** A snapshotted `{ tag, async fetch(req) { return new Response(this.tag + ':' + req.method) } }`
called three ways:

| receiver | outcome |
| --- | --- |
| the `default` object (parent of the last segment) | `200`, body `"obj-a:POST"` — **correct** |
| `globalThis` | `200`, body `"undefined:POST"` — **silently wrong** |
| `undefined` | rejected: `TypeError: Cannot read properties of undefined (reading 'tag')` |

The important half is the middle row. Getting the receiver wrong does not fail
loudly; it returns a `200` with corrupted content. Any handler that reads `this`
— a class instance, a closure-free object literal, anything holding
configuration — produces wrong output rather than an error.

**Options.**

- **A — receiver is the object the final segment was read from** (`globalThis`
  for a single-segment path). Exactly JavaScript's `a.b.c()` semantics.
- **B — receiver is always `globalThis`.** Simpler to implement, and produces the
  silent corruption above.
- **C — receiver is `undefined`** (strict-mode-like). Fails loudly rather than
  silently, but breaks every method-shaped handler, which is the shape the
  product is for.

**Recommendation: A.** It is the only option that makes `export default { fetch }`
work, and it is what any JavaScript author will predict. Two edge cases to
document rather than support:

- `export default class Worker { … }` makes `default.fetch` a prototype method
  with no instance. Recommend a clean error rather than implicit construction:
  the supported shapes are `export default { fetch }` and
  `export default new Worker()`.
- A path resolving to a bound function or an arrow function ignores the receiver
  by language rule. That is correct behaviour, not a bug, but it is worth one
  line in the docs because it will look like the receiver decision failed.

### 4.4 Async

**The decision.** A `fetch` handler returns a Promise, so this cannot be a bare
`Function::call`. How much of `run_module_inner` does the call path need?

`internal/API_DECISIONS.md` flagged its own answer here as *reasoned from the
code, not measured* — so it was measured. **The claim holds, and the cost
estimate attached to it was wrong in a useful direction.**

**Measured.** A snapshot-restored handler invoked via `Function::call`, with the
returned promise driven by the same loop `run_module_inner` uses (microtask
checkpoint, settle one parked bridge call, re-inspect the promise). A native stub
standing in for a bridge global parks its resolver exactly as
`bridge_global_callback` does:

| handler shape | poll rounds needed | p50 µs |
| --- | ---: | ---: |
| sync, returns a `Response` (no promise at all) | 0 | 7.8 |
| `async`, no awaits | 0 | 7.3 |
| `async`, 3 awaits, no bridge call | **1 checkpoint, 0 resolves** | 12.6 |
| `async`, 2 bridge calls | 2 | 8.5 |
| `async`, 10 bridge calls | 10 | 10.1 |

Three findings:

1. **The loop is structurally required and nearly free.** Ten bridge rounds cost
   1.6 µs more than two — roughly **0.2 µs per round**. What makes a bridge call
   expensive is the socket (§5.7: ~29 µs), never the loop.
2. **Rounds track bridge round trips, not awaits.**
   `perform_microtask_checkpoint()` drains the whole queue, so three sequential
   `await`s with nothing to wait *for* settle in a single checkpoint. A design
   that budgeted one poll iteration per `await` would be sized wrong.
3. **A sync handler needs no loop at all.** `Local::<Promise>::try_from` on the
   return value is the whole fast path; when it fails, serialize and return.

Decomposition of the realistic case (async handler, reads its body, builds a
`Response`), warm isolate, p50:

| leg | µs |
| --- | ---: |
| deserialize the `Request` args blob (175 B) | 4.42 |
| `Function::call` up to the first await | 2.04 |
| poll loop | 2.67 |
| serialize the `Response` result blob | 1.58 |
| **total** | **10.7** |

Note the largest single leg is deserializing a 175-byte `Request` — ~7× what a
plain object of comparable size costs, because rehydration runs the `Request` and
`Headers` constructors. Not a problem at 4.4 µs; worth knowing before anyone
optimises the wrong leg.

**What the call path must keep** from `run_module_inner`: microtask checkpoints,
the `BridgeResponse` socket read and `callId` routing, `bridge_error` inspection
at both checkpoints, the two guard threads and the `TerminationReason` → error
mapping, `Terminate` handling for graceful abort (`DESIGN.md` §14.7), and
CPU-budget `enter()`/`leave()` bracketing around the socket read.

**What it can drop:** `compile_module`, `instantiate_module`, `evaluate`, the
module-resolver install, and module-namespace extraction. ~8.5 µs.

**What it must *not* drop:** `install_console` and `install_async_context`.
Skipping them is worth ~18 µs and changes the execution contract — `console.*`
throws, `node:async_hooks` stops resolving. A request-handling product wants both.

**Recommendation.** Factor the poll loop out of `run_module_inner` so both entry
points drive the same code, with the settle condition parameterised (a module's
top-level promise, or a call's return value). Skip the loop entirely when the
return value is not a Promise. One behaviour to decide explicitly: a returned
promise that never settles and has no bridge call in flight. With a socket
attached this blocks until the wall guard fires and reports
`ERR_WALL_TIMEOUT`, which is defensible; the alternative is detecting quiescence
and reporting a distinct `ERR_CALL_UNRESOLVED_PROMISE`, mirroring the existing
`ERR_EXPORT_UNRESOLVED_PROMISE`.

### 4.5 Isolate lifetime

**The decision.** One call per isolate (today's boundary) or many calls per
isolate (the Workers model)?

**Measured.** Same handler, same argument blob, Rust side, p50:

| | µs per call |
| --- | ---: |
| 1 call per isolate | 1,097 |
| 10 calls per isolate | 137.8 |
| 100 calls per isolate | 25.6 |

And the reason the curve is that steep — cost of each call by index, inside one
freshly restored isolate:

| | µs |
| --- | ---: |
| restore + context (no dispose) | 654 |
| **call #1** | **221.4** |
| call #2 | 38.6 |
| call #3 | 16.8 |
| call #4 | 15.5 |
| call #5 | 18.8 |
| call #6 | 15.2 |

**This is a new finding and it is the most important number in this RFC.** The
first call into a snapshot-restored callable costs **14× the steady-state call**,
even with `FunctionCodeHandling::Keep`. A per-request isolate pays that 221 µs
every single request, on top of the ~654 µs it already pays to stand the isolate
and context up. (Cause reasoned, not measured: lazy deserialization of snapshotted
function code plus first-run feedback-vector and inline-cache setup for the
handler and the web-runtime classes it touches.)

So the "the call itself is 606 ns, therefore free" framing is only true in a warm
isolate. On a one-shot isolate the call is ~221 µs.

**Options.**

- **A — one call per isolate.** ~1.1 ms per request in Rust, ~1.4 ms
  host-observed (§5.4). An earlier session measured the aggregate ceiling for
  this shape at ~9,000 runs/sec on ten cores. Per-request isolation. No new
  decisions, no new invariants broken.
- **B — persistent per-tenant isolate.** 25.6 µs per call in Rust; end-to-end
  adds the socket and Node's own ~35 µs, so call it **tens of µs** — a 20–40×
  improvement, not the ~600 ns the raw `Function::call` figure suggests.
  Breaks `DESIGN.md` §7.6 ("no shared state between runs"): a tenant's handler
  retains module state across requests. That is simultaneously the feature
  (connection pools, warm caches, exactly why Workers does it) and the risk
  (cross-request leakage *within* a tenant, unbounded memory growth, one
  poisoned isolate serving many requests). The rusty_v8 constraints —
  `OwnedIsolate` is not `Send`, `Drop` asserts reverse-order disposal, `Locker`
  is not exposed — pin an isolate to one session thread, so the connection
  effectively *is* the tenant.
- **C — bounded reuse:** recycle after N calls or T seconds idle. At N=10 the
  amortised cost is already 8× better than N=1; bounded state accumulation, a
  natural memory-leak backstop, and a tunable isolation dial.

**Recommendation: take this question out of the call-API decision.**

The call API is an **argument channel**. It changes no lifecycle and breaks no
invariant, and it is equally correct with A, B, or C. Isolate reuse is a
**product decision** that rewrites `DESIGN.md` §7.6 and §13, and by constraint 4
it belongs to the request-handling product rather than to `@iso4/sandbox`.
Bundling them means the small, safe change waits on the large, contentious one.

If the request-handling product does get its own answer, the recommendation there
is **C** — it captures most of the 40× and keeps a bound on everything B makes
unbounded.

*Correction to the framing in the internal notes:* per-request **database
opening** is not what forces isolate reuse. Under every SQLite option in §6 the
connection lives in a process that outlives the isolate (Node, or the Rust
runtime), so DB lifetime is already decoupled from isolate lifetime. What forces
the question is the isolate itself: ~490 µs of construction plus the 221 µs
first-touch penalty above.

### 4.6 Frame shape

**The decision.** New `Call` / `CallResult` frames, or an optional `exportPath` +
`argsBlob` on the existing `PrefixRun`?

**Facts.** Next free type bytes are `0x08` (TS → Rust) and `0x06` (Rust → TS);
the current tables are `0x01`–`0x07` and `0x01`–`0x05` respectively. Protocol v2
is unreleased (§2.4), so an additive field on an existing v2 frame is free right
now and a v3 bump later. `RunCompletionPayload` already carries everything a call
result needs: the value blob, captured stdout/stderr, `durationMs`, `cpuTimeMs`,
and the bridge-call records.

**Options.**

- **A — new `Call` (0x08) / `CallResult` (0x06) frames.** Cleanest conceptual
  separation. Costs a second result path, and every piece of machinery hung off
  `Result` has to be re-hung: the graceful-abort flow (`DESIGN.md` §14.7) that
  matches a `Terminate` to a `Result { ERR_ABORTED }`, telemetry, the
  bridge-record settle path, `drainUntilResult` on the TS side.
- **B — extend `PrefixRun`.** Add
  `call: Optional<{ String exportPath, ValueBlob args }>` and make `code`
  `Optional<String>`. When `call` is present the runtime resolves and invokes
  instead of compiling. The entire result path, error mapping, abort handling and
  telemetry are reused unchanged. Much smaller diff.
- **C — a discriminated union inside `PrefixRun`** — a `u8` mode tag with a
  mode-specific tail. Structurally honest about "exactly one of these"; a larger
  wire change than B for the same behaviour.

**Recommendation: B**, landed inside the v2 window. Concretely:
`code: Optional<String>`, `call: Optional<CallTarget>`, and **exactly one of the
two must be present** — reject both-present and neither-present with
`ERR_INTERNAL`-class validation at parse time. Making it exclusive is not
tidiness: it is what guarantees no sandbox code runs in the isolate before the
call target is resolved, which is what makes the writable stash slot in §4.1
safe.

One thing B does *not* get for free: a `Run` (non-prefix) call has nothing to
call into, since there is no snapshot and therefore no stash. Recommend that a
`call` on the `Run` frame is a validation error, and that calls are a
prefix-only capability. That is also the honest product statement — you call into
something you prepared.

### 4.7 Errors

**The decision.** Does a throw inside the called function map to the existing
run-failure shape, or to a distinct call-failure shape?

**Facts.** A throw inside a called handler is the same class of event as an
uncaught exception in postfix code, and the existing machinery already handles it
completely: `runtime_error_from_value` carries name, message, stack and extra
fields; `host_bridge_error_from_rejection` already classifies an uncaught
host-handler error as `ERR_HOST_BRIDGE`. Nothing new is needed for the throw
itself.

What *is* genuinely new is target resolution. Four failure modes:

| failure | existing code that fits |
| --- | --- |
| the handler throws / its promise rejects | `ERR_USER_CODE` — exact fit |
| the args blob will not deserialize | `ERR_TYPE_NOT_SERIALIZABLE` — fits |
| the return value will not serialize | `ERR_EXPORT_NOT_SERIALIZABLE` — fits, but the name now reads oddly |
| `exportPath` does not resolve, or resolves to a non-callable | **nothing fits** |

**Options.**

- **A — reuse `RunErrorPayload` plus one new code** for target resolution.
- **B — a distinct call-failure envelope.** Duplicates the error vocabulary for
  no observable gain, and doubles the four-place sync burden that `AGENTS.md`
  rule 11 imposes on every new error code (`RunError` enum,
  `run_error_to_payload`, `RunErrorCode` union, `docs/protocol.md` §7).

**Recommendation: A**, with a single new code — `ERR_CALL_TARGET_NOT_FOUND` —
covering both "path does not resolve" and "resolved value is not callable", with
the message distinguishing them. One code rather than two keeps the sync burden
minimal and matches how `ERR_UNDECLARED_BINDING` already covers several distinct
binding mistakes.

Leave `ERR_EXPORT_NOT_SERIALIZABLE` as-is for an unserialisable return value.
Renaming it would be a breaking change to a published union for a cosmetic gain;
one sentence in `docs/protocol.md` §7 noting it also covers call return values is
enough.

---

## 5. Measurements

Everything in this section was measured for this RFC. Numbers are marked
**measured** or **reasoned**; nothing is carried over from another session
without saying so.

### 5.1 Conditions

Release binary (`node scripts/build-native.ts --release`, `lto = "fat"`,
`codegen-units = 1`), 10-core Apple Silicon, rusty_v8 130, Node 26.5, one
session. Rust-side numbers come from a throwaway `src/bin/` spike; end-to-end
numbers from a throwaway vitest file driving the real subprocess over the real
socket at `maxIsolates: 1`. Both deleted after measuring, per the repo's
convention for spikes.

**Caveat, and it matters.** The machine carried a load average of ~1.4 during the
session, not idle. This project has a documented history of a **uniform ~1.6×
inflation** of absolute V8 execution figures on a loaded machine, reproducible
with background CPU burners, spanning benchmarks from 39 ns to 485 µs. So:

- **Ratios and A/B comparisons taken within this session are sound** and are what
  every recommendation above rests on.
- **Absolute µs figures here must not be compared against numbers measured on an
  idle machine** — including the 487 µs run-tax table in §2.2. Where this RFC
  needs a floor to subtract against, it measures its own (`S0`, `B0`) inside the
  same session.

Every table below reports p50 over 120–300 iterations after 20–50 warm-up
iterations.

### 5.2 Per-call cost, warm isolate

Snapshot: 546,510 B (web runtime + handlers). `Request` args blob: 175 B.
See §4.4 for the decomposition and §4.2/§4.3 for addressing and receiver.

| handler | p50 µs |
| --- | ---: |
| sync, returns a `Response` | 7.8–12.7 |
| `async`, no awaits | 7.3 |
| `async`, reads body, builds a `Response` | 11.3–12.6 |
| class instance method (`default.fetch`) | 7.9 |
| object literal method (`default.fetch`) | 8.0 |

### 5.3 Per-request cost, one-shot isolate (Rust side)

Restore snapshot → context → do the work → dispose:

| | p50 µs | Δ vs floor |
| --- | ---: | ---: |
| B0 floor: restore + context + dispose, no work | 864.0 | — |
| B1 today: recompile a postfix module per request | 1,314.6 | +450.6 |
| B2 call API: resolve + call per request | 1,104.2 | +240.2 |

The B1 − B2 gap, **210 µs**, is the whole per-request saving a call API offers
over recompiling a small postfix — and §5.5 shows most of it disappears once you
compare against the shape a host would actually use today.

### 5.4 End-to-end, host-observed, through the real socket

One HTTP-shaped request handled by a prepared prefix. The handler reads its
body, parses JSON, and returns a `Response`.

| shape | p50 µs | needs a protocol change? |
| --- | ---: | --- |
| S0 floor: `prefix.execute('export default 1')` | 954.7 | — |
| S1 request interpolated into the postfix source | 1,425.8 | no |
| S2 request **pulled over the bridge**, fixed postfix | 1,368.6 | no |
| S3 request constructed inside the sandbox (no argument at all) | 1,445.8 | no |
| S4 handler source *and* request compiled per request | 1,426.1 | no |
| S5 handler source compiled per request, request pulled | 1,362.1 | no |

Two results worth stating plainly:

- **The pull model is the cheapest argument channel available today**, by ~60 µs,
  and it is consistent in direction across all three trials (S1: 1438/1428/1426;
  S2: 1357/1374/1369). It adds a bridge round trip (~29 µs) and still wins,
  because a fixed postfix avoids building the `Request` in JS — the host →
  sandbox rehydration walk constructs it natively instead. (The ~60 µs total is
  measured; that explanation for it is **reasoned**.)
- A real host `Request` crossed into the sandbox through a bridge response and
  was fully usable — `await request.text()`, `request.url`, `request.method` all
  worked. §2.3's capability is confirmed end-to-end.

### 5.5 A premise that does not survive measurement

The internal notes describe the recompile-per-request shape as costing
**~0.33 ms/request, roughly a third of per-request cost**, and call it the
sharpest concrete argument for the call API.

**For a small handler that is not reproducible.** S4 (handler source compiled
every request) and S1 (handler in the snapshot, only the request per request)
are **1,426.1 vs 1,425.8 µs** — identical. Compiling a ten-line handler costs
~5 µs, which is invisible at this scale. What *is* ~0.48 ms is the fetch-shaped
work itself (S3 − S0 = 491 µs): constructing a `Request`, reading its body,
JSON, constructing a `Response`, serializing it — of which ~221 µs is the
first-touch penalty from §4.5, which no argument channel removes.

**For a realistic handler the premise is right, but it argues for something
else.** "Pure-JS libraries" means the handler arrives as a bundle:

| bundle size | compiled per request | baked into the snapshot | delta |
| --- | ---: | ---: | ---: |
| 54 KB | 2,271.3 µs | 1,505.6 µs | **+765.7 µs (+51 %)** |
| 216 KB | 5,144.2 µs | 1,980.3 µs | **+3,163.9 µs (2.6×)** |

At roughly 25 KB of handler source the penalty is indeed ~0.33 ms. So the
measurement behind the claim is real — but the fix it argues for is *putting the
handler in the snapshot*, and **that already works today**: the 1,505.6 µs and
1,980.3 µs columns are the existing `globalThis` stash plus the §5.4 pull model,
with no protocol change.

Against that baseline a call frame replaces one bridge round trip (~29 µs) and
one tiny postfix compile (~5 µs): **~34 µs of a ~1,500 µs request, about 2 %.**

**Conclusion, and it is the same one the internal notes reached by a different
route:** the case for the call API is **capability, not performance**. It is the
prerequisite for `export default { fetch(request) }` as a first-class shape, for
addressing named handlers, and for passing real typed arguments without
interpolation or a pull hop. The performance argument should be dropped from the
case rather than repeated, because at every handler size it lands around 2 %.

(Also visible in that table: baking a bundle into the snapshot is not free
either — 1,505.6 → 1,980.3 µs as the bundle grows 54 → 216 KB, because
`Isolate::new` scales with snapshot content. Consistent with §2.2's warning about
data-heavy prefixes, and it means "bake everything" has its own ceiling.)

### 5.6 Isolate lifetime

See §4.5. The amortisation curve (1,097 → 137.8 → 25.6 µs/call at 1/10/100 calls
per isolate) and the first-touch penalty (221.4 µs on call #1, 15–19 µs at steady
state) are the numbers that decision turns on.

### 5.7 The bridge round trip, and the native alternative

A handler issuing N sequential bridge queries per request, each returning three
small rows, end-to-end:

| queries | p50 µs |
| ---: | ---: |
| 0 | 1,479.8 |
| 1 | 1,521.5 |
| 5 | 1,697.0 |
| 10 | 1,858.5 |
| 25 | 2,322.6 |
| 50 | 3,001.7 |
| 100 | 4,415.2 |

**Marginal cost per bridge query: 29.4 µs.** A handler issuing 100 queries spends
**2.9 ms crossing the boundary, on top of a 1.5 ms request** — the DB boundary
becomes 2× everything else combined. (The internal notes estimated ~19 µs/query
and ~2 ms; 19 µs comes from loop-mode throughput, a different and more favourable
shape. 29 µs is the figure for a query issued from inside a handler.)

The in-isolate native alternative, measured in a tight JS loop:

| | ns per call |
| --- | ---: |
| pure JS loop, no call (baseline) | 5.96 |
| JS → native Rust callback, returns an int | 13.54 |
| JS → native Rust callback, builds a `{id, name}` row | 308.36 |

Two things follow, and the second is easy to miss:

1. **The native call boundary is ~7.6 ns** (13.54 − 5.96) — about **4,000×**
   cheaper than a bridge round trip, and 5× cheaper than the ~40 ns the internal
   notes estimated.
2. **Materialising results is the real cost, not the call.** A two-property row
   built from Rust costs ~308 ns, because each `Object::set` across the public V8
   API is ~87 ns. A 100-row × 4-column result set is ~120 µs of property sets —
   *four bridge round trips' worth*. So "native is 4,000× cheaper" is true of the
   call and false of the result. Any native data path should return large result
   sets as **one V8 blob** rather than per-value `set` calls: `ValueDeserializer`
   runs *inside* V8 with pre-sized property stores and no API boundary, which is
   the same finding that made the v8-blob codec 2.6× faster than any external
   codec could be.

---

## 6. The adjacent decision: the SQLite access path

This is the bigger fork, and §5.7 is the evidence. It should be decided before
the call API, because unlike the call API it can move an architectural rule.

**The framing.** Cloudflare keeps SQLite in-process with the isolate (D1,
Durable Objects) for exactly the reason §5.7 measures. `AGENTS.md` says
two-process is the only backend. Those two facts look like a head-on collision.
They are not, and the reason is worth being precise about: the two-process rule
exists to keep **untrusted V8 out of the Node process**, so that an OOM or a V8
bug cannot take down the host. It says nothing about what else may live in the
*Rust runtime* process.

**Options.**

- **A — SQLite behind the bridge, in Node.** Zero architectural change; it is
  just another host global. **29 µs per query.** Fine for a handler issuing a
  handful of queries; fatal for one issuing a hundred. Available today with no
  iso4 change at all.
- **B — SQLite native, inside the Rust runtime process**, exposed to the isolate
  as a native global or host import. Query cost drops to the native boundary
  (~7.6 ns) plus the actual query plus result materialisation (§5.7, point 2).
  **Does not violate the two-process rule**: untrusted JS still runs in a
  separate process from Node, and crash isolation for the host is unchanged.
  What it *does* change:
  - The runtime process links a C library and holds database file handles in the
    same address space as untrusted JS. A memory-safety bug in SQLite becomes an
    escape from the isolate to the runtime process — not to Node. That is a
    smaller blast radius than option C but a real widening of the sandbox
    process's trusted surface.
  - **Per-tenant scoping becomes the runtime's job.** Today all authorization
    lives in host handlers on the Node side; a native DB global moves an
    authorization decision inside the sandbox process. This is the substantive
    cost of B, and it is a design problem rather than a performance one.
  - Native callbacks must survive the prefix snapshot, which needs an
    `ExternalReferences` table — **now proven** by PR #52 (§2.3). This was the
    blocker; it is gone.
- **C — SQLite in-process with Node.** Cloudflare's shape. Violates the rule as
  written, and their precedent does not transfer: workerd owns its whole stack
  and has no Node process to protect.
- **D — batched / prepared-statement bridge API.** One crossing per batch rather
  than per query. Recovers most of the 29 µs at the cost of the developer-facing
  API: no `db.query()` per row, and no ORM. For a product whose pitch is "host
  simple request/response web functions", that is a real DX regression, and it
  does not help a handler that pulls in a query builder.

**Recommendation: B, with A as the interim.** A is available now, needs no
decision, and is correct for handlers issuing few queries — ship the product on
it and learn what real query counts look like. B is where the product has to end
up if the answer is "dozens per request", and the two decisions it forces
(trusted-surface widening, per-tenant scoping inside the runtime) deserve their
own design pass rather than being smuggled in with a call API.

Explicitly **not** recommended: deciding this by choosing D to avoid the
architecture conversation. D trades a measured 29 µs for an unmeasured amount of
developer friction, and the friction is the product.

---

## 7. What workerd does, and what transfers

`docs/protocol.md` and `internal/` already cite workerd as the closest prior art.
For this RFC specifically:

**Transfers.**

- **`export default { fetch }` as the addressing model**, with the receiver being
  the default export object. §4.3 measures why the receiver half is not optional.
- **Hand-written per-type serializers, versioned by a tag list** (`JSG_SERIALIZABLE`),
  with no generic class-instance mechanism. Already adopted in PR #52
  (`docs/protocol.md` §4.4), including workerd's own warning that generic
  class-instance serializers over RPC "have a history of creating security bugs".
- **Keeping the hot data path in-process with the isolate** — the reasoning
  behind §6, though the conclusion has to differ because iso4 has a Node process
  to protect and workerd does not.

**Does not transfer.**

- **Their boundary is a live capability-passing RPC connection**; iso4's is a
  one-shot frame. workerd's `fetch(request)` can pass stubs, streams and
  promise-pipelined capabilities. iso4 cannot, by constraints 1 and 2 — which is
  also why `docs/protocol.md` §4.4.5 excludes streaming bodies and reserves tags
  4–6 for the day that changes.
- **`SetWriteVersion`**, which lets them pin the V8 serialization format both
  directions, is a Cloudflare V8 patch and not upstream. iso4 hard-fails at the
  handshake instead.
- **Their isolate lifetime.** Workers reuse isolates across requests as a
  premise. For iso4 that is §4.5, and it is a product decision this repo has
  explicitly deferred to a separate product.

---

## 8. Contradictions found in the existing docs

Flagged per the standing instruction. One is fixed in this PR; the rest are
reported only.

1. **`DESIGN.md` §6.2's message tables are wrong — fixed in this PR.** They list
   four TS → Rust types (`Authenticate` 0x01, `Run` 0x02, `BridgeResponse` 0x03,
   `Terminate` 0x04) and four Rust → TS types including a **`StdioChunk`** frame.
   The real protocol has seven TS → Rust types (0x01–0x07) and five Rust → TS
   types (0x01–0x05), and `native/v8-runtime/src/ipc.rs` states outright that
   "there is no `StdioChunk` in the real protocol — stdout/stderr are captured by
   Rust and included inside the `Result` payload". `docs/protocol.md` §2 is
   correct. Fixed, because §6.2 is precisely the table §4.6 would extend, and
   leaving a wrong frame table next to an RFC that proposes new frames invites
   a collision.
2. **`DESIGN.md` §13.4 allocates 0x05 / 0x06 / 0x07 to `OpenSession` / `Call` /
   `CloseSession`.** Those bytes are now `DisposePrefix` / `BridgeResponse` /
   `Terminate`. The section is marked archived, but the byte allocation is
   actively misleading to anyone designing new frames — which is the exact
   audience of §4.6. Recommend deleting the byte table from the archived section
   or marking it superseded.
3. **`console` capture is documented as an unresolved design item but has
   shipped.** `DESIGN.md` §3 step 3 ("Log capture (`console.*`) is a separate
   design item and must not be smuggled in as an ad-hoc inline prelude"),
   `DESIGN.md` §10 ("How exactly should logs/stdout/stderr be captured?" listed
   as open), and `AGENTS.md` ("Log handling (`console.*`) is still a TODO") all
   predate `install_console` and the captured stdout/stderr arrays in
   `RunCompletionPayload`. Three places to correct.
4. **`v8.rs`'s `precompile` doc comment gives a reason that is no longer true.**
   It says `console` is unavailable in prefix code because "native callbacks
   cannot be snapshotted without `ExternalReferences` (a Phase 2 concern)". PR
   #52 shipped a working `ExternalReferences` table. The *behaviour* is unchanged
   — `install_console` still runs per-run and was never moved into the snapshot —
   but the stated blocker is gone, and the comment now reads as a constraint when
   it is a to-do.
5. **`DESIGN.md` §11.5 says snapshot restore costs "~1–2 ms".** Measured this
   session: a whole `prefix.execute()` that does nothing is 954.7 µs
   host-observed, of which restore + context + dispose is ~864 µs in Rust. The
   estimate is pessimistic by roughly 2×, on a loaded machine at that. Minor.
6. **`DESIGN.md` §7.6 ("No shared state between runs. Each `run()` is a fresh
   `v8::Context`.")** is currently accurate and would be **falsified** by
   choosing option B or C in §4.5. Not a contradiction today; noted because it is
   the specific sentence that decision rewrites.
7. **Cosmetic numbering damage.** `DESIGN.md` §15 is titled "Callable handles
   (Phase 13)" but its subsections are numbered `### 14.1`–`### 14.6`, and §13
   has two subsections numbered 13.3 and two numbered 13.4. `AGENTS.md`'s
   architecture tree still shows a `packages/iso4/` package and marks
   `native/v8-runtime/` and the platform packages as "(planned)". Several
   `AGENTS.md` bullets are also truncated mid-sentence (items 1, 18, and the
   `DESIGN.md` bullet in "Maintaining Documentation").

---

## 9. Summary

| # | Decision | Recommendation | Why |
| - | -------- | -------------- | --- |
| 1 | Where the callable lives | Auto-stash the prefix namespace under a reserved **string** key at `prepare()` | Only option that keeps the ESM-exports idiom; **no hidden slot exists** — `v8::Private` does not survive the snapshot |
| 2 | Addressing | Dotted string, resolved per call, ≤4 segments, resolved after `cpu_budget.enter()` | Handle registry saves 0.2 µs of a ~1,400 µs request and costs a lifecycle |
| 3 | Receiver | Parent of the final path segment (`a.b.c()` semantics) | `globalThis` produces a **`200` with corrupted content**, measured — silent, not loud |
| 4 | Async | Share one parameterised poll loop; skip it when the return is not a Promise | Loop is required but ~0.2 µs per round; rounds track bridge trips, not awaits |
| 5 | Isolate lifetime | **Remove from this decision.** Bounded reuse if the request product asks | Argument channel breaks no invariant; reuse rewrites `DESIGN.md` §7.6 and §13 |
| 6 | Frame shape | Extend `PrefixRun` with `Optional<CallTarget>`; exactly one of `code`/`call`; prefix-only | Reuses the whole result/abort/telemetry path, and v2 is still unreleased |
| 7 | Errors | Reuse `RunErrorPayload` + one new `ERR_CALL_TARGET_NOT_FOUND` | A handler throw is already an `ERR_USER_CODE`; only target resolution is new |
| — | **SQLite access path** | **B** (native in the Rust runtime process), **A** as the interim | 29 µs/query measured — 100 queries is 2× the whole request; B keeps two-process intact |

And the one thing to carry out of §5: **the call API should be argued for as a
capability, not as a performance win.** At every handler size measured it is
worth about 2 % of a request. The capability — `export default { fetch(request) }`,
named handlers, real typed arguments without interpolation — is sufficient
justification on its own, and it is the justification that survives measurement.
