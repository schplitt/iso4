# Analytics Engine — Design Notes & Performance Reference

Captured after the first round of benchmarking (debug build, macOS arm64,
`packages/iso4-dynamic/bench/runtime.bench.ts`). Revisit these numbers once
a release build lands.

---

## 1. Benchmark baseline (debug build)

| Scenario                                                              | Min     | Mean    | P99     |
| --------------------------------------------------------------------- | ------- | ------- | ------- |
| **cold / direct** `createRuntime → run → dispose`                     | 53.8 ms | 56.9 ms | 59.6 ms |
| **cold / prefix** `createRuntime → precompile → prefix.run → dispose` | 63.5 ms | 65.5 ms | 68.6 ms |
| **hot / direct** `runtime.run`                                        | 0.41 ms | 0.50 ms | 0.73 ms |
| **hot / prefix** `prefix.run`                                         | 0.42 ms | 0.49 ms | 0.54 ms |

**Context:**

- Cold cost is paid once per `createRuntime()` call — binary spawn + V8
  platform init. Not a per-run cost.
- Hot `runtime.run` at ~0.5 ms includes full ESM compilation of the postfix
  on every call. The Rust logs consistently show "run succeeded in 0ms",
  meaning V8 execution of `export default 42` is negligible — the 0.5 ms is
  IPC + ESM compile.
- Hot `prefix.run` is marginally faster because the prefix state is
  snapshot-restored (no re-running prefix code), but the postfix is still
  compiled on every call. The gap will widen as prefix code grows (libraries,
  tool bindings, etc.).
- **Release build makes no measurable difference** (confirmed: `cargo build
  --release` + re-bench produced identical numbers within noise). Both
  measurements are dominated by code outside our Rust wrapper: cold start
  by V8's own platform init (pre-compiled inside rusty_v8's static lib),
  hot run by the UDS kernel round trip and V8's ESM JIT. Our Rust frame
  parsing/encoding is already in the single-digit µs range — invisible
  against the 500 µs total. These are the real floor for the two-process
  architecture on this hardware. Moving them requires architectural changes
  (see §3), not build flags.

**Comparison point:** A colleague's QuickJS-WASM setup pays ~16 ms per run
(startup included each time). iso4 pays ~57 ms once, then ~0.5 ms per call —
roughly 32× faster per call for any workload with more than one run.

---

## 2. The two use cases

### 2.1 AI agent / dynamic code (iso4-dynamic)

- Code changes per call (agent-generated).
- Each run: postfix compiled fresh → execute → serialize exports → done.
- Isolation critical — untrusted code, must survive crashes.
- Concurrency via pool: N callers each get their own slot and run in parallel.
- **Hot path at ~0.5 ms is already fine for interactive agents.**
  Even at 10 concurrent agents, 0.5 ms/call is invisible compared to LLM
  inference and tool I/O.
- A cluster of multiple Rust processes _may_ become necessary at very high
  agent concurrency (hundreds of simultaneous runs), but a single binary with
  `maxIsolates` equal to CPU count handles typical MCP multi-agent workloads
  without any additional complexity.

### 2.2 Analytics / static transforms (iso4-static)

- Code is **fixed** — a small set of known templates (e.g. `transform`,
  `aggregate`, `validate`). No dynamic code.
- Each call: invoke a named export with an input object, get a value back.
- No host imports, no `fetch`, no bridge APIs — pure JS computation only.
- Isolation still desirable but crash impact is contained by the outer
  container (Docker/K8s).
- **Raw throughput is the only metric that matters.**

---

## 3. Performance tiers

| Architecture                             | Per-call Cost | Throughput (N=8 Slots) | How                           |
| ---------------------------------------- | ------------- | ---------------------- | ----------------------------- |
| `runtime.run()` hot (current)            | ~0.5 ms       | ~16k calls/s           | IPC + ESM compile per call    |
| `session.call()` two-process (Phase 11)  | ~50–150 µs    | ~50–160k calls/s       | IPC only, no compile per call |
| `iso4-static` in-process NAPI (Phase 11) | ~1–5 µs       | ~1–8M calls/s          | No IPC, direct V8 invocation  |

The IPC overhead (UDS round trip) is the fundamental ceiling for any
two-process architecture. More workers parallelise the work but do not reduce
per-call latency. The only way past ~150 µs/call is eliminating the socket.

**Target workload estimate:**

- DB-write pipeline: DB write is typically 0.5–5 ms. Transform throughput
  ceiling should be at least 10× the DB write rate.
- If DB writes at 10k rows/s → transforms need ~1M rows/s → NAPI path.
- If DB writes at 1k rows/s → transforms need ~100k rows/s → session model
  (two-process Phase 11) with a small worker pool is sufficient.

---

## 4. The cluster / multi-worker approach

### What it is

N instances of the same Rust binary, each with its own UDS socket, managed by
a single Node host process. A TS-side router distributes `prefix.call()` calls
across workers.

### When it makes sense

- **Agent workloads at scale**: single binary runs out of OS threads or
  memory at very high concurrency (hundreds of parallel agent runs). A cluster
  spreads load across N binaries. Each binary is independently crashable.
- **Tenant isolation**: give each customer/workspace their own binary so
  one tenant's runaway code can't starve others.
- **Stepping stone**: if the NAPI work isn't done yet but throughput beyond
  one binary is needed.

### Prefix replication strategy

Keep it simple:

1. **Prefer** a free worker that already has the prefix snapshot in memory.
2. If no such worker is free, route to **any** free worker and compile there
   (lazy replication). The extra compile cost is paid once per worker, not
   per call.
3. No active push/broadcast of snapshots — let demand drive replication.
   A prefix that's called on all workers will naturally replicate to all
   workers over time.

This is not maximally efficient (occasionally a worker re-compiles a snapshot
another already has), but it is simple and correct. The compile cost is
small relative to the call volume that follows.

### What it does NOT solve

A cluster of Rust processes still crosses a UDS boundary per call. At 8
workers each handling calls at ~150 µs (session model), you get ~50k calls/s.
That may or may not be enough — see §3. If it isn't, the cluster buys
operational complexity without reaching the target; go straight to `iso4-static`.

### API sketch

```ts
// Hypothetical — not yet designed or built
const cluster = await createRuntimeCluster({
  workers: 4, // N Rust processes, N sockets
  maxIsolatesPerWorker: 8, // 32 total concurrent slots
})

const prefix = await cluster.precompile({ code: transformSrc })
// Routes to any worker; lazily replicates to others as they become the
// preferred worker for subsequent calls.

const result = await prefix.call('transform', row)
// Routed to a free worker that has the snapshot (preferred) or any free
// worker (fallback, compile on that worker, cache for next time).
```

---

## 5. iso4-static design intent

`@iso4/static` (`createStaticRuntime`) is the right answer for the pure
analytics use case. Key design points already captured in
`packages/iso4-static/src/types.ts`:

- **In-process via NAPI** — no UDS, no separate binary. Direct V8 function
  invocation inside the Node process.
- **Isolate pool per prefix** — `maxConcurrent` slots, each a snapshot-restored
  V8 isolate. `prefix.call()` acquires a slot, invokes the named export,
  returns the result, releases the slot.
- **No async bridge** — calls are synchronous from the isolate's perspective.
  Input and output cross via V8 `ValueSerializer`. No `fetch`, no host imports,
  no callbacks.
- **Container is the security boundary** — no crash isolation from the host
  process. Run inside Docker/K8s.
- **Same API surface as the dynamic prefix** — `precompile()` + `prefix.call()`
  instead of `prefix.run()`. Swappable behind the same options type.

**Implementation prerequisite:** `rusty_v8` cannot be used in-process (it
calls `v8::V8::initialize_platform()` which Node already did). `iso4-static`
requires a C++ NAPI addon that uses Node's existing V8 headers — the same
approach as `isolated-vm`. This is non-trivial but is the proven path.

---

## 6. Recommended sequence

1. **Now** — `iso4-dynamic` Phase 3–8 (CPU budget, fetch bridge, imports,
   memory limits). Gets the dynamic runtime to v1.
2. **Phase 11** — `session.call()` on the two-process backend. Unlocks the
   ~150 µs/call tier without any NAPI work. Evaluate whether this is fast
   enough for the target analytics workload.
3. **Phase 11 / iso4-static** — NAPI in-process backend. Build if Phase 11
   session model doesn't hit the throughput target. Start with a minimal C++
   addon (precompile + call, no bridge, no fetch).
4. **Cluster (if needed)** — Design after Phase 11. Only build it if tenant
   isolation or very high agent concurrency (not raw analytics throughput)
   is the driving requirement.

---

## 7. Open questions

- **Target row volume?** Determines whether Phase 11 two-process session model
  is enough or whether the NAPI path is required.
- **Acceptable tail latency?** p99 matters more than mean for DB-write
  pipelines. UDS p99 jitter (~3× mean in current benchmarks) may be
  unacceptable at high volume even if mean is fine.
- **Template count?** If there are O(10) fixed templates, eager replication
  across all cluster workers at startup is practical. If templates are
  user-defined and numerous, lazy replication is the only viable approach.
- **Input/output object size?** V8 `ValueSerializer` is fast for small
  objects but serialization cost grows with payload size. Large input rows
  (>10 KB) will shift the bottleneck from compute to serialization.
- **Cluster vs iso4-static priority?** If the container boundary is acceptable
  as the security model, skip the cluster and go straight to iso4-static.
  If crash isolation at the process level is required even for analytics,
  the cluster is necessary.
