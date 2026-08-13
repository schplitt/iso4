# @iso4/sandbox

Fast, sandboxed V8 isolate runtime for agent-generated JavaScript. Runs user
code in a separate Rust process for full crash isolation — an OOM or panic in
the sandbox kills only the subprocess, not your host application.

Built for the AI-agent prefix/postfix pattern: precompile host setup (globals,
libraries, tool bindings) once, then run many agent-generated code strings
against it in parallel — each run in a fresh, clean isolate.

> **Status:** core execution works end-to-end. Not yet at 1.0.

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

const sandbox = await createSandbox({ memoryMb: 128 }) // uniform heap cap per isolate

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
  limits: { cpuTimeMs: 200, wallTimeMs: 5_000 }, // memoryMb is a createSandbox option
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
    // memoryMb is uniform per isolate and set on createSandbox({ memoryMb }),
    // not per run (#64) — the heap cap is baked into the isolate at creation.
    maxBridgeCalls: 10, // max host-bridge calls per run (0 = unlimited)
    maxBridgePayloadBytes: 0, // max bytes per bridge call (0 = 64 MiB framing cap)
  },
})
```

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
}
```

`run()` never throws for sandboxed failures — only for infrastructure errors
(subprocess crashed, binary not found). `ok: false` with an error code is the
normal failure path.

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

## Architecture

V8 runs in a separate Rust subprocess communicating over a Unix domain socket.
A pool of connections (one per `maxIsolates` slot) provides concurrency —
five concurrent `prefix.execute()` calls each get their own slot and execute in
parallel.

## License

MIT
