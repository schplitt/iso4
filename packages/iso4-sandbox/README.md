# @iso4/sandbox

Fast, sandboxed V8 isolate runtime for agent-generated JavaScript. Runs user
code in a separate Rust process for full crash isolation — an OOM or panic in
the sandbox kills only the subprocess, not your host application.

Built for the AI-agent prefix/postfix pattern: prepare host setup (globals,
libraries, tool bindings) once, then run many agent-generated code strings
against it in parallel. Every run gets its own limits, guards, and bridge
bindings; one-off `sandbox.run()` always gets a fresh isolate, while prefix
runs reuse resident warm instances when one is free (see below).

> **Status:** core execution works end-to-end. Not yet at 1.0 — every API,
> option, and observability surface (including `stats()`) may still change.

## Install

```sh
npm i @iso4/sandbox
# hardened fetch defaults (recommended):
npm i @iso4/fetch
```

## Quick start

```ts
import { createSandbox } from '@iso4/sandbox'
import { createSafeFetch } from '@iso4/fetch'

const sandbox = await createSandbox({ memoryMb: 128 }) // default heap cap per isolate

// Validate and prepare host setup once
const prefix = await sandbox.prepare({
  code: `
    const config = { apiBase: 'https://api.example.com' }
    globalThis.config = config
  `,
  globals: {
    fetch: createSafeFetch({ policy: ({ host }) => host === 'api.example.com' }),
  },
})

// Run agent-generated code against the prefix — as many times as needed
const result = await prefix.execute({
  code: `
    const res = await fetch(config.apiBase + '/users')
    export default { count: res.length }
  `,
  limits: { cpuTimeMs: 200, wallTimeMs: 5_000 }, // heap cap: prepare({ memoryMb }) or the sandbox default
})

if (result.ok) {
  console.log(result.exports.default) // { count: 42 }
} else {
  console.error(result.error.code, result.error.message)
}

await sandbox.dispose()
```

> `sandbox.prepare()` and `prefix.execute()` are the current names. The former
> names — `sandbox.precompile()` and `prefix.run()` — remain as deprecated
> aliases with identical behavior and will be removed in a future major.

## How globals work

`globals` wires any non-reserved name directly into the sandbox's global
object as a bridge stub. The bridge is fully generic — `fetch` is not
special-cased:

```ts
const options = {
  globals: {
    searchWeb: async (query: string) => {
      const res = await fetch(`https://api.example.com/search?q=${encodeURIComponent(query)}`)
      return res.json()
    },
  }
}
```

Functions in bridge return values are currently dropped — return plain data,
not class instances with methods.

## TypeScript-checked rebinding

`prepare()` infers the globals shape `G` from what you pass, and the
returned `Prefix<G>` only allows rebinding those names at run time:

```ts
const prefix = await sandbox.prepare({
  globals: { fetch: defaultFetch, myTool: defaultTool },
})

prefix.execute({ globals: { fetch: perUserFetch } }) // ✅ rebind one
prefix.execute({ globals: { unknown: handler } }) // ❌ TS error
```

## Resource limits

```ts
prefix.execute({
  code: agentCode,
  limits: {
    cpuTimeMs: 200, // active JS execution only (await time excluded)
    wallTimeMs: 5_000, // hard cap including async waits
    // memoryMb: per prefix via prepare({ memoryMb }) or the sandbox default —
    // never per prefix-run, the cap is baked into the shared warm isolates.
    // One-off sandbox.run() may set limits.memoryMb (fresh isolate anyway).
    maxBridgeCalls: 10, // max host-bridge calls per run (0 = unlimited)
    maxBridgeCallBytes: 0, // max bytes per bridge call (0 = 64 MiB framing cap)
    maxExportBytes: 0, // max bytes of encoded exports (0 = unlimited)
    maxStdoutBytes: 0, // per-run console capture caps (0 = unlimited)
    maxStderrBytes: 0,
  },
})
```

## Warm instances and the memory budget

Runs against a prepared prefix are served by **warm instances**: resident
isolates with the prefix already evaluated, so boot and prefix evaluation
are paid once instead of per run. Automatic — no flag, no separate API.

Warmth is a cache, never a guarantee. An instance can vanish between any
two calls (memory pressure, `dispose()`), and a run interrupted
**mid-execution** — a CPU or memory limit firing, or an abort landing on
actively running code — costs its instance: the next call cold-starts
clean. Failures that arrive while a run is _waiting_ (a wall timeout during
a host call, an abort of a suspended run, a dropped connection) fail that
run alone and the instance keeps serving, state intact. Don't rely on state
carrying over either way: `globalThis` writes and patched builtins stay
visible to later runs until eviction silently wipes them. Keep durable
state in a database and do expensive setup lazily in the handler
(`conn ??= await connect()`).

Carryover is not an isolation boundary. Because a warm instance is reused
across runs of the same prefix, one run can change what a later run sees —
including reassigning the runtime's own globals (`Response`, `fetch`, …) or
patching prototypes, which then affects that later run. This is intended and
matches a shared-isolate worker: run one prefix only for callers that trust
each other, and give mutually-distrusting workloads separate prefixes.

Instances of one prefix share no state with each other; today each serves
one call at a time, so concurrency means more instances (the engine can
already interleave several runs on one instance — that switches on with
wire multiplexing). Top-level names never collide across runs
(each run is its own module). A prefix that can't finish evaluating under
the fixed warm-up budget is rejected by `prepare()` with
`ERR_WARMUP_LIMIT`. `console.*` from prefix evaluation arrives on the
cold-starting call's result.

Residency is bounded by memory, not by a count:

```ts
const sandbox = await createSandbox({
  memoryMb: 128, // default heap cap per isolate (override per prefix at prepare())
  memoryBudgetMb: 2048, // RSS mark for the whole runtime process (0 = off)
})
```

The runtime watches its own process RSS. At or above `memoryBudgetMb` it
evicts idle instances (largest heap × longest idle first) and stops adding new
warm ones — prefix runs then execute on cold one-off isolates — until RSS falls
back to 80 % of the mark. `maxConcurrentRuns` caps concurrent runs; this caps
memory.
The default is derived from the memory available to the process
(container-aware), so most hosts never set it.

`sandbox.stats()` reports the live picture — active runs, queue depth, warm and
idle instance counts, summed idle heap, `budgetBytes` / `rssBytes`, whether the
runtime is currently `underPressure`, and per-prefix counts. It answers on a
dedicated connection, so it works even when every run slot is busy.

## Async context (`AsyncLocalStorage`)

Run/postfix code can import a minimal, Node-compatible `AsyncLocalStorage` to
carry an ambient value across `await` points — concurrency-safe, unlike a
module variable:

```ts
prefix.execute({
  code: `
    import { AsyncLocalStorage } from 'node:async_hooks'
    const als = new AsyncLocalStorage()
    export default await als.run('trace-42', async () => {
      await somethingAsync()
      return als.getStore()   // 'trace-42', even several awaits deep
    })
  `,
})
```

Only `run(store, callback, ...args)` and `getStore()` are provided. Built on
V8's continuation-preserved embedder data; no promise hooks, so it's free
unless used. Not available in `prepare()` (prefix) code — it's for the
postfix. See DESIGN.md §16.

## Calling into the sandbox

Invoke a function the module exports — `export default { fetch }` or any
named export — with real typed arguments. On a prepared prefix nothing is
compiled per request:

```ts
const prefix = await sandbox.prepare({ code: workerBundle })

const result = await prefix.call({
  export: 'default.fetch',
  args: [new Request('https://example.com/', { method: 'POST', body: 'hi' })],
})
if (result.ok) {
  const response = result.value as Response // a real Response instance
}
```

`sandbox.run({ code, call })` does the same against a freshly evaluated
module. With `call` the success result carries the function's return `value`
instead of `exports` — never both. The receiver is the exported object
(`this` works); prototype methods resolve (`export default new Worker()`),
and a path that does not reach a callable fails with
`ERR_CALL_TARGET_NOT_FOUND`. `prefix.call()` accepts the same per-call
`globals` / `imports` rebinds as `prefix.execute()`.

Non-serializable **exports** no longer fail a plain run: they are absent from
`exports` and reported in `skippedExports`, so a module that exports handlers
still reads cleanly. `sandbox.readExports({ code })` wraps that for the
deploy path — load once, read the declaration exports, and get the skipped
handler names back.

## Result shape

```ts
type RunResult
  = | { ok: true, exports: SandboxExports, skippedExports: string[], stdout: string[], stderr: string[], durationMs: number, cpuTimeMs: number, bridgeCalls: BridgeCallEntry[] }
    | { ok: false, error: RunError, stdout: string[], stderr: string[], durationMs: number, cpuTimeMs: number, bridgeCalls: BridgeCallEntry[] }

// prefix.call() / run({ code, call }) resolve to a CallResult instead:
// the success arm carries `value` (the function's return value) in place of
// `exports` + `skippedExports`; failure/aborted arms are shared.

// durationMs — wall-clock time of the run; cpuTimeMs — active V8 execution
// time (bridge waits excluded). Both measured in the runtime, µs resolution.

interface BridgeCallEntry { // recorded in the Rust runtime; one per attempt, in order
  name: string // 'fetch', 'myTool', or '<specifier>.<path>' for host-module imports
  startMs: number // offset from run start (same clock as durationMs)
  durationMs: number // round-trip the sandbox waited (handler + IPC)
  argBytes: number // serialized call payload size
  responseBytes: number // serialized response value size (0 on handler error)
  ok: boolean
  blocked: boolean // blocked by a limit runtime-side; never reached the host
}

interface RunError {
  code: RunErrorCode
  name: string
  message: string
  stack?: string
  fields?: Record<string, unknown> // all other own-enumerable props of the thrown error
  resetCause?: 'cpu' | 'memory' | 'wall' | 'abort' | 'internal' // ERR_INSTANCE_RESET only
  culpritRunId?: number // ERR_INSTANCE_RESET only: the run whose interruption reset the instance
}
```

`run()` never throws for sandboxed failures — only for infrastructure errors
(subprocess crashed, binary not found). `ok: false` with an error code is the
normal failure path.

One code is about a _neighbor_, not your own run: `ERR_INSTANCE_RESET` means a
run sharing the same warm instance had to be terminated mid-execution
(`resetCause` says why, `culpritRunId` names it), so this run — in flight on
the now-untrusted instance — was failed with its real partial telemetry. It is
never retried automatically, because it may already have had side effects.

Thrown errors keep their identity across the bridge, in both directions:

- **Sandbox → host**: an uncaught sandbox throw surfaces as `ERR_USER_CODE`
  with the error's real `name`, `message`, `stack`, and every other
  own-enumerable property under `error.fields` (namespaced so a custom `code`
  property can't collide with the iso4 `error.code`). `name`/`message`/`stack`
  are reserved and never appear inside `fields`.
- **Host → sandbox**: a host handler that throws rejects the sandbox call with
  a real `Error` carrying the same `name` (`instanceof TypeError` works for
  built-ins) and its extra properties re-attached directly (`e.status`,
  `e.reason`, …). Sandbox code can catch it and continue; uncaught it fails
  the run with `ERR_HOST_BRIDGE`. The **host stack never crosses** into the
  sandbox.

> Every own property you attach to a host-thrown error is visible to sandbox
> code (the auto-populated stack is the one exception). Sandbox code is
> untrusted, and this includes properties a third-party SDK attaches to its own
> error objects — so if an error may carry request context or credentials
> (an SDK's `config`/`request`/`response`), throw a clean `Error` from your
> handler instead of re-throwing it wholesale. Sanitising is the handler's job,
> the same as with any value you return.

## Architecture

V8 runs in a separate Rust subprocess communicating over a Unix domain socket.
A slot pool admits up to `maxConcurrentRuns` runs at once (the rest queue
FIFO — an `AbortSignal`, e.g. `AbortSignal.timeout()`, bounds the wait);
connections to the subprocess open on demand and are reused, and `stats()`
reports the live count as `openConnections`. Five concurrent
`prefix.execute()` calls each get their own run slot and execute in
parallel.

## License

MIT
