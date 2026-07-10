# @iso4/sandbox

Fast, sandboxed V8 isolate runtime for agent-generated JavaScript. Runs user
code in a separate Rust process for full crash isolation — an OOM or panic in
the sandbox kills only the subprocess, not your host application.

Built for the AI-agent prefix/postfix pattern: precompile host setup (globals,
libraries, tool bindings) once into a V8 startup snapshot, then run many
agent-generated code strings against the snapshot in parallel.

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

const sandbox = await createSandbox()

// Compile host setup once into a V8 snapshot
const prefix = await sandbox.precompile({
  code: `
    const config = { apiBase: 'https://api.example.com' }
    globalThis.config = config
  `,
  globals: {
    fetch: createSafeFetch({ policy: ({ host }) => host === 'api.example.com' }),
  },
})

// Run agent-generated code against the snapshot — as many times as needed
const result = await prefix.run({
  code: `
    const res = await fetch(config.apiBase + '/users')
    export default { count: res.length }
  `,
  limits: { cpuTimeMs: 200, wallTimeMs: 5_000, memoryMb: 64 },
})

if (result.ok) {
  console.log(result.exports.default) // { count: 42 }
} else {
  console.error(result.error.code, result.error.message)
}

await sandbox.dispose()
```

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

`precompile()` infers the globals shape `G` from what you pass, and the
returned `Prefix<G>` only allows rebinding those names at run time:

```ts
const prefix = await sandbox.precompile({
  globals: { fetch: defaultFetch, myTool: defaultTool },
})

prefix.run({ globals: { fetch: perUserFetch } }) // ✅ rebind one
prefix.run({ globals: { unknown: handler } }) // ❌ TS error
```

## Resource limits

```ts
prefix.run({
  code: agentCode,
  limits: {
    cpuTimeMs: 200, // active JS execution only (await time excluded)
    wallTimeMs: 5_000, // hard cap including async waits
    memoryMb: 64, // V8 heap + ArrayBuffer budget
    maxBridgeCalls: 10, // max host-bridge calls per run (0 = unlimited)
    maxBridgePayloadBytes: 0, // max bytes per bridge call (0 = 64 MiB framing cap)
  },
})
```

## Result shape

```ts
type RunResult
  = | { ok: true, exports: SandboxExports, stdout: string[], stderr: string[], durationMs: number }
    | { ok: false, error: RunError, stdout: string[], stderr: string[], durationMs: number }

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
five concurrent `prefix.run()` calls each get their own slot and execute in
parallel.

## License

MIT
