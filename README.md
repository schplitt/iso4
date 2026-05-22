# iso4

> Fast, sandboxed V8 isolate runtime with permission-controlled `fetch` and pluggable modules.
> Built for the AI-agent prefix/postfix pattern.

A minimal, composable successor to heavyweight Node-compatible sandboxes. Pure JavaScript
execution in a separate Rust V8 process. Memory and CPU limits enforced at the V8 level.
Async time spent waiting on host work doesn't count against the CPU budget. Host code decides
what `fetch` can reach and what modules the sandbox can `import` — nothing leaks unless it's
handed over by name.

> **Status:** early development. The architecture is committed (see
> [`DESIGN.md`](./DESIGN.md), [`MONOREPO.md`](./MONOREPO.md), and
> [`packages/iso4/src/types.ts`](./packages/iso4/src/types.ts)) but the
> runtime is not yet implemented.

## Why

The canonical pattern this is built for: host application sets up tools, data, and library
bindings as a **prefix**; an AI agent generates a **postfix** that uses them; the host
executes the concatenation and gets a result. The host wants:

- Hard limits on memory and active execution time per run.
- Fine control over which URLs and which modules the sandbox can reach.
- Sub-5 ms cold start once the prefix is compiled.
- A small, auditable runtime — no Node-stdlib emulation, no kernel, no virtual POSIX.

## Quick example

```ts
import { createRuntime } from 'iso4'
import { createSafeFetch } from '@iso4/fetch'

const runtime = await createRuntime()

const prefix = await runtime.precompile({
  code: `
    const config = { apiBase: 'https://api.example.com' }
    globalThis.config = config
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

## Packages

| Package | Status | Description |
|---|---|---|
| [`iso4`](./packages/iso4) | scaffolding | Core runtime, host API, IPC, sandbox shims |
| [`@iso4/fetch`](./packages/iso4-fetch) | scaffolding | Hardened `FetchHandler` with DNS pin, allowlist, SSRF blocks |
| `@iso4/fs` | future | `node:fs` stub factory with configurable root + permissions |
| `@iso4/crypto` | future | `node:crypto` stub factory (safe parts only) |
| `@iso4/v8-<platform>` | planned | Per-platform Rust V8 binaries (built from `native/v8-runtime/`) |

See [`MONOREPO.md`](./MONOREPO.md) for the package boundary rules.

## Documentation

- [`DESIGN.md`](./DESIGN.md) — architecture, execution model, limits, security model, build plan
- [`MONOREPO.md`](./MONOREPO.md) — package layout, dependency direction, versioning, distribution
- [`AGENTS.md`](./AGENTS.md) — guidelines for agents working in this repo
- [`packages/iso4/src/types.ts`](./packages/iso4/src/types.ts) — canonical public API surface

## Development

Requires Node 24+ and pnpm. Mise pins the toolchain (`mise.toml`).

```sh
pnpm install        # bootstrap workspace
pnpm build          # build all packages
pnpm test:run       # run all tests (no watch)
pnpm test           # run vitest in watch mode
pnpm lint           # eslint
pnpm lint:fix       # eslint --fix
pnpm typecheck      # tsc --noEmit across packages
pnpm changeset      # record a per-package version bump for a PR
```

Releases use [changesets](https://github.com/changesets/changesets) for
independent per-package versioning. See [`.changeset/README.md`](./.changeset/README.md)
for the workflow.

## License

[MIT](./LICENSE) © schplitt
