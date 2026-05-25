# iso4

Fast, sandboxed V8 isolate runtime with permission-controlled `fetch` and
pluggable modules. Built for the AI-agent prefix/postfix pattern: precompile
host setup once, run generated postfix code many times against the snapshot.

> **Status:** early development. The API surface is committed in
> [`./src/types.ts`](./src/types.ts) but the runtime is not yet implemented.
> See [`../../DESIGN.md`](../../DESIGN.md) and
> [`../../MONOREPO.md`](../../MONOREPO.md) for the plan.

## Install

```sh
npm i iso4
# add hardened fetch defaults:
npm i @iso4/fetch
```

## Quick example (planned API)

```ts
import { createRuntime } from 'iso4'
import { createSafeFetch } from '@iso4/fetch'

const runtime = await createRuntime()

const prefix = await runtime.precompile({
  code: `
    const config = { apiBase: 'https://api.example.com' }
    function callTool(name, args) {
      return globalThis._tool(name, args)
    }
    globalThis.config = config
    globalThis.callTool = callTool
  `,
  globals: {
    fetch: createSafeFetch({ allowedHosts: ['api.example.com'] }),
  },
})

const result = await prefix.run({
  code: `
    const res = await fetch(config.apiBase + '/users')
    const data = await res.json()
    export default { count: data.length }
  `,
  limits: { cpuTimeMs: 200, memoryMb: 64 },
})

if (result.ok) {
  console.log(result.exports.default) // { count: 42 }
}
```

## What this package contains

- The TypeScript host API (`createRuntime`, `Runtime`, `PrecompiledPrefix`, …)
- IPC client + frame codec for the Rust V8 process
- Bridge dispatch for host-configured globals/functions and module imports
- Mechanical fetch hygiene when the host chooses to expose `fetch`
  (header/URL/method validation, body size caps)
- Runtime-owned sandbox shims where explicitly designed. Log handling
  (`console.*`) is still a TODO; do not infer an inline prelude shim.

## What this package does not contain

- An HTTP client. `fetch` is not special-cased as an always-present runtime
  global; expose it through the bridge when needed. Use `@iso4/fetch` for a
  hardened handler.
- Any policy. Allow/deny decisions live in handlers the host supplies.
- Any Node-stdlib emulation. Use `@iso4/fs`, `@iso4/crypto`, etc. (planned)
  for stub factories.

## License

MIT
