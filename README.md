# iso4

> Fast, sandboxed V8 isolate runtime with permission-controlled `fetch` and pluggable modules.
> Built for the AI-agent prefix/postfix pattern.

A minimal, composable V8 sandbox. Pure JavaScript execution in a separate Rust process.
Memory and CPU limits enforced at the V8 level. Async time spent waiting on host work
doesn't count against the CPU budget. Host code decides what `fetch` can reach and what
modules the sandbox can `import` — nothing leaks unless it's handed over by name.

> **Status:** core execution works end-to-end. Not yet at 1.0.

## Why

The canonical pattern this is built for: host application sets up tools, data, and library
bindings as a **prefix**; an AI agent generates a **postfix** that uses them; the host
executes the postfix and gets a result. The host wants:

- Hard limits on memory and active execution time per run.
- Fine control over which URLs and which modules the sandbox can reach.
- Sub-5 ms cold start for typical prefixes (validated once, evaluated per run).
- A small, auditable runtime — no Node-stdlib emulation, no kernel, no virtual POSIX.

## Quick example

```ts
import { createSandbox } from '@iso4/sandbox'
import { createSafeFetch } from '@iso4/fetch'

const sandbox = await createSandbox({ memoryMb: 128 }) // uniform heap cap per isolate

const prefix = await sandbox.precompile({
  code: `
    const config = { apiBase: 'https://api.example.com' }
    globalThis.config = config
  `,
  globals: {
    fetch: createSafeFetch({
      rules: {
        host: 'api.example.com',
        routes: [{ path: '/users/**', methods: 'GET' }],
      },
    }),
  },
})

const result = await prefix.run({
  code: `
    const res = await fetch(config.apiBase + '/users')
    const users = res.json()
    export default { count: users.length }
  `,
  limits: { cpuTimeMs: 200 }, // memoryMb is set on createSandbox, not per run
})

if (result.ok) {
  console.log(result.exports.default) // { count: 42 }
}

await sandbox.dispose()
```

## Calling into the sandbox

`export default { fetch(request) }` is a first-class shape: prepare the
module once, then call its exported handler per request with real typed
arguments — a `Request` crosses in, a `Response` crosses back, and the result
carries the handler's return `value` instead of `exports`:

```ts
const prefix = await sandbox.prepare({ code: workerBundle })

const result = await prefix.call({
  export: 'default.fetch', // or any named export, e.g. 'handleEvent'
  args: [new Request('https://example.com/in', { method: 'POST', body: 'hi' })],
})

if (result.ok) {
  const response = result.value as Response
  console.log(response.status, await response.text())
}
```

The same works on a direct run (`sandbox.run({ code, call })`), resolved
against the freshly evaluated module. The receiver is the exported object
itself, so handlers reading `this` behave normally; a path that does not
resolve to a callable fails with `ERR_CALL_TARGET_NOT_FOUND`.

For the deploy path, `sandbox.readExports({ code })` loads a module once and
returns its serializable exports (IaC-style declarations); function-valued
exports are absent and reported in `skippedExports` — never an error:

```ts
const { exports, skippedExports } = await sandbox.readExports({ code: workerBundle })
console.log(exports.limits) // { memoryMb: 128 }  — declaration exports
console.log(skippedExports) // ['default']        — the handler object, skipped
```

## Packages

| Package                                    | Status  | Description                                                                                                               |
| ------------------------------------------ | ------- | ------------------------------------------------------------------------------------------------------------------------- |
| [`@iso4/sandbox`](./packages/iso4-sandbox) | working | Subprocess V8 sandbox — crash-isolated, host bridge, prepared prefixes                                                    |
| [`@iso4/fetch`](./packages/iso4-fetch)     | working | Hardened `fetch` for sandbox globals: DNS pinning, SSRF blocking, route-based allowlist, middleware, redirect re-checking |
| `@iso4/embed`                              | future  | In-process V8 sandbox for high-throughput trusted code (NAPI, no crash isolation)                                         |
| `@iso4/fs`                                 | future  | `node:fs` stub factory with configurable root + permissions                                                               |
| `@iso4/crypto`                             | future  | `node:crypto` stub factory (safe subset)                                                                                  |
| `@iso4/v8-<platform>`                      | working | Per-platform Rust binaries for `@iso4/sandbox` (built in CI, not committed)                                               |

## Development

Requires Node 24+ and pnpm. Mise pins the toolchain (`mise.toml`).

```sh
pnpm install            # bootstrap workspace
pnpm build:dev          # build native binary (debug) + all TS packages
pnpm build              # build native binary (release) + all TS packages
pnpm test:run           # cargo test (Rust) + vitest across all packages
pnpm lint               # eslint
pnpm lint:fix           # eslint --fix
pnpm typecheck          # tsc --noEmit across all packages
pnpm changeset          # record a per-package version bump
```

## Current status

- `sandbox.run({ code, limits })` — direct sandboxed execution ✅
- `sandbox.precompile()` + `prefix.run()` — prefix/postfix pattern ✅
- CPU and wall-clock limits enforced, async wait excluded from CPU budget ✅
- Host-declared globals bridged into V8 (`fetch`, `myTool`, any name) ✅
- `ERR_UNDECLARED_BINDING` enforced on `prefix.run()` globals ✅
- Host-provided imports — source modules (`string`) and host modules (objects with function/data leaves) resolved via `import` in sandbox code ✅
- `AbortSignal` support — cancel in-flight runs; `RunResult.status` discriminates `'completed'` | `'failed'` | `'aborted'`, with `reason` on abort ✅
- Error propagation — thrown error `name`, `stack`, and extra enumerable properties (`error.fields`) survive the bridge in both directions ✅
- Bridge call limits — `maxBridgeCalls`, `maxBridgeCallBytes`, `maxExportBytes` per run ✅
- Host → sandbox calls — `prefix.call({ export, args })` / `run({ code, call })` invoke exported handlers (`default.fetch`, named) with real typed arguments ✅
- `sandbox.readExports()` — deploy-path declaration reader; non-serializable exports skipped and reported via `skippedExports` ✅
- `@iso4/fetch` — rules-based origin + route allowlist, three-level middleware, DNS pinning, SSRF/redirect protection ✅

## How globals work

`globals` wires any non-reserved name directly into the sandbox's global
object. The bridge is completely generic — the name `fetch` is not special:

```ts
const result = await sandbox.run({
  code: `
    const data = await searchWeb('cats')
    export default data.results
  `,
  globals: {
    searchWeb: async (query: string) => {
      const res = await fetch(`https://api.example.com/search?q=${encodeURIComponent(query)}`)
      return res.json()
    },
  },
})
```

The handler receives the raw arguments from sandbox code and must return
plain serializable data. **Functions in return values are currently dropped.**

## Async context (`AsyncLocalStorage`)

Sandboxed code can carry an ambient value across `await` points without
threading it through every call — and, unlike a module-level variable,
concurrent async chains stay isolated. Imported the Node way:

```ts
const result = await sandbox.run({
  code: `
    import { AsyncLocalStorage } from 'node:async_hooks'
    const keyScope = new AsyncLocalStorage()

    // A durable-workflow style step: each nested step appends to the key,
    // so a step nested inside another never collides with the same name used
    // elsewhere.
    function step(name, body) {
      const parent = keyScope.getStore() ?? ''
      return keyScope.run(parent ? parent + '/' + name : name, body)
    }

    let innerKey
    await step('charge', async () => {
      await step('validate', async () => {
        await Promise.resolve()
        innerKey = keyScope.getStore()   // 'charge/validate'
      })
    })
    export default innerKey
  `,
})
// result.exports.default === 'charge/validate'
```

Only `run(store, callback, ...args)` and `getStore()` are provided — the
concurrency-safe core. It's built on V8's continuation-preserved embedder data
(the same primitive modern Node uses), registers no promise hooks, and is
always available to run code at no cost unless used. It is **not** available in
`precompile()` (prefix) code — it's for the postfix. See DESIGN.md §16.

## License

[MIT](./LICENSE) © schplitt
