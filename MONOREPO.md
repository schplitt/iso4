# iso4 — Monorepo layout

The reason this is a monorepo and not a single package: the **runtimes are
small, opinionated, and rarely change**. Everything composed on top of them
— hardened fetch, stdlib stub factories (`@iso4/fs`, `@iso4/crypto`, …),
tool-calling helpers, agent integrations — has its own release cadence,
its own dependency set, and its own surface area. Bundling them all into
one package would force every consumer to pull in deps they don't use and
would couple unrelated release cycles.

The rule we follow: **a package exists when it has a different consumer or
a different set of trust assumptions than the runtime.**

Naming convention: every package is scoped under `@iso4/`. The scope makes
ownership obvious and reserves the namespace.

---

## 1. Package map

```
iso4/
  DESIGN.md
  MONOREPO.md
  README.md
  package.json                      ← workspace root, no publishable contents
  pnpm-workspace.yaml
  .changeset/                       ← per-package version bumps
  packages/
    iso4-core/                      ← @iso4/core          (shared types, no runtime code)
    iso4-dynamic/                   ← @iso4/dynamic       (two-process Rust runtime)
      src/                          ← TypeScript host API
      v8-runtime/                   ← Rust source for the V8 host binary
      tests/
    iso4-static/                    ← @iso4/static        (in-process napi-rs runtime, Phase 11)
      src/                          ← TypeScript API (scaffolded, not yet implemented)
    iso4-fetch/                     ← @iso4/fetch         (hardened FetchHandler)
    iso4-fs/                        ← @iso4/fs            (future, node:fs stub factory)
    iso4-crypto/                    ← @iso4/crypto        (future, node:crypto stub factory)
    iso4-v8-darwin-arm64/           ← @iso4/v8-darwin-arm64      (native binary for @iso4/dynamic)
    iso4-v8-darwin-x64/             ← @iso4/v8-darwin-x64
    iso4-v8-linux-x64-gnu/          ← @iso4/v8-linux-x64-gnu
    iso4-v8-linux-arm64-gnu/        ← @iso4/v8-linux-arm64-gnu
    iso4-static-darwin-arm64/       ← @iso4/static-darwin-arm64  (native binary for @iso4/static, Phase 11)
    iso4-static-darwin-x64/         ← @iso4/static-darwin-x64
    iso4-static-linux-x64-gnu/      ← @iso4/static-linux-x64-gnu
    iso4-static-linux-arm64-gnu/    ← @iso4/static-linux-arm64-gnu
  examples/
    minimal/                        ← @iso4/dynamic, no fetch, no imports
    agent-prefix-postfix/           ← canonical AI loop with precompile
    safe-fetch/                     ← @iso4/fetch in action
    analytics-pipeline/             ← @iso4/static in action (Phase 11)
  bench/                            ← startup latency, throughput benchmarks
```

---

## 2. Packages

### 2.1 `@iso4/core` — shared types

Types only. No runtime code, no native dependencies. Every other package
in the ecosystem depends on this.

Owns:

- `ResourceLimits` — memory/CPU/wall limits used by both runtimes.
- `FetchHandler`, `HostFetchRequest`, `HostFetchResponse` — the fetch
  interface that `@iso4/fetch` implements and both runtimes consume.
- `HostGlobals` — the allowlisted globals object (currently just `fetch`).
- `ImportsConfig`, `ImportDefinition`, `SourceImport`, `HostImport`,
  `HostExports`, `HostExportValue/Data/Function` — the module resolver
  surface shared by both runtimes.

Does **not** own result types (`RunResult`, `CallResult`) — those are
specific to each runtime's execution model.

### 2.2 `@iso4/dynamic` — two-process dynamic runtime

The runtime for dynamic (agent-generated) code. Every `run()` compiles a
fresh code string and executes it in a fresh isolate from a V8 snapshot.
Crash-isolated via a separate Rust process.

Owns:

- TypeScript host API: `createRuntime`, `Runtime`, `DynamicPrefix`, and
  all supporting types. Re-exports shared types from `@iso4/core`.
- Spawning the per-platform Rust binary (via `optionalDependencies` on
  `@iso4/v8-*` packages).
- The IPC client, frame codec, and UDS connection pool.
- Bridge dispatch for `fetch` and host-module imports.
- `RunResult`, `RunSuccess`, `RunFailure`, `SandboxExports` — the ESM
  export-based result type specific to this runtime.

The Rust source for the V8 host binary lives at
`packages/iso4-dynamic/v8-runtime/` and is built once per release per
platform via CI. The compiled binary ships in the corresponding
`@iso4/v8-<platform>` package.

**Security model**: V8 runs in a separate OS process. An OOM or crash in
the isolate kills only the Rust subprocess; the host process and all other
concurrent runs are unaffected.

### 2.3 `@iso4/static` — in-process static runtime (Phase 11)

The runtime for static (precompiled) code called with varying data. The
prefix is compiled once; `prefix.call('fn', input)` invokes a named export
with no recompilation and no fresh-isolate overhead. Uses napi-rs to create
V8 isolates in-process via Node's existing V8 platform.

Owns:

- TypeScript host API: `createStaticRuntime`, `StaticRuntime`,
  `StaticPrefix`, `CallResult`. Re-exports shared types from `@iso4/core`.
- Loading the per-platform napi-rs `.node` binary (via `optionalDependencies`
  on `@iso4/static-*` packages).
- Internal isolate pool management.
- `CallResult`, `CallSuccess`, `CallFailure` — the function-return-value
  result type specific to this runtime.

The Rust/napi-rs source will live at `packages/iso4-static/napi-runtime/`
(Phase 11). Compiled `.node` binaries ship in `@iso4/static-<platform>`
packages, mirroring the `@iso4/v8-*` pattern.

**Security model**: V8 runs in the Node process. An OOM can crash the host
process. **Requires Docker/Kubernetes or equivalent as the outer security
boundary.** See DESIGN.md §1.2.

Not yet implemented. Scaffolded types only.

### 2.4 `@iso4/fetch` — hardened fetch defaults

Optional but strongly recommended for `@iso4/dynamic` deployments.

Owns:

- `createSafeFetch(options)` returning a `FetchHandler` ready to plug into
  `globals.fetch` of either runtime.
- DNS pre-resolution with IP pinning (prevents DNS rebinding).
- Allowlist/denylist matching.
- Private/link-local IP blocking (RFC1918, loopback, link-local, ULA).
- Redirect re-checking.
- An isolated `undici` Dispatcher (no shared state with the host app).

Depends on `@iso4/core` (peer) for `FetchHandler` and related types.
Does **not** depend on `@iso4/dynamic` or `@iso4/static` — it works with
both and has no knowledge of which runtime is in use.

### 2.5 `@iso4/v8-<platform>` — native binaries for @iso4/dynamic

One package per supported platform. Each contains a single compiled Rust
binary plus a tiny `index.js` that exports the binary path.
`@iso4/dynamic` lists these as `optionalDependencies`; pnpm/npm installs
only the one matching `process.platform + process.arch`.

Platforms: `darwin-arm64`, `darwin-x64`, `linux-x64-gnu`, `linux-arm64-gnu`.

### 2.6 `@iso4/static-<platform>` — native binaries for @iso4/static (Phase 11)

Same pattern as `@iso4/v8-*` but for the napi-rs `.node` addon.
`@iso4/static` lists these as `optionalDependencies`.

Platforms mirror `@iso4/v8-*`.

### 2.7 Stdlib-stub packages (`@iso4/fs`, `@iso4/crypto`, …)

Tiny factory packages that produce a `HostImport` matching the shape of a
Node.js stdlib module, ready to plug into `imports.static` of either runtime.

```ts
import { createRuntime } from '@iso4/dynamic'
import { createFsModule } from '@iso4/fs'

const runtime = await createRuntime()
const prefix = await runtime.precompile({
  code: prefixSource,
  imports: {
    static: {
      'node:fs': createFsModule({ root: '/sandbox', readOnly: true }),
    },
  },
})
```

Planned (not in v1): `@iso4/fs`, `@iso4/crypto`, `@iso4/path`,
`@iso4/url`, `@iso4/buffer`.

---

## 3. Dependency direction

```
                    @iso4/core
              ▲       ▲       ▲
              │       │       │
    @iso4/dynamic  @iso4/static  @iso4/fetch
              ▲
    @iso4/v8-*  (optional, runtime-resolved)

    @iso4/static
              ▲
    @iso4/static-* (optional, runtime-resolved, Phase 11)
```

Rules:

- **`@iso4/core` depends on nothing** in this monorepo.
- **`@iso4/dynamic` and `@iso4/static` depend on `@iso4/core`** for
  shared types; never on each other.
- **`@iso4/fetch` depends on `@iso4/core`** (peer) only; never on a runtime.
- **Sibling stdlib-stub packages do not depend on each other.**

---

## 4. Versioning

Independent per package, via `@changesets/cli`. Every user-visible PR adds
a changeset file declaring which packages bump and at what level.

What changes in lockstep:

- Wire-protocol changes between `@iso4/dynamic` (TS) and `@iso4/v8-*`
  (Rust). Both bump major. Protocol version `u16` in the `Authenticate`
  frame prevents mismatched-binary foot-guns.
- Same rule for `@iso4/static` ↔ `@iso4/static-*` when Phase 11 lands.

---

## 5. Build and dev workflow

```sh
pnpm install               # bootstrap
pnpm build                 # build all TS packages
cd packages/iso4-dynamic/v8-runtime && cargo build --release   # Rust binary
pnpm test:run              # vitest across all packages (single-shot, CI-safe)
pnpm changeset             # record a version bump
```

---

## 6. What goes where: a quick reference

| If It's About…                                              | Put It In                                     |
| ----------------------------------------------------------- | --------------------------------------------- |
| Types shared between runtimes and/or @iso4/fetch            | `@iso4/core`                                  |
| The V8 isolate, UDS, IPC framing, snapshot, bridge dispatch | `@iso4/dynamic/v8-runtime/` (Rust)            |
| The TypeScript host API for dynamic code                    | `@iso4/dynamic/src/`                          |
| The napi-rs in-process isolate pool                         | `@iso4/static/napi-runtime/` (Rust, Phase 11) |
| The TypeScript host API for static code                     | `@iso4/static/src/`                           |
| Validating headers/URLs/methods are well-formed             | `@iso4/dynamic` (mechanical hygiene)          |
| Deciding which URLs are allowed                             | `@iso4/fetch` or host app code                |
| Hardened HTTP client (DNS pin, redirect re-check, etc.)     | `@iso4/fetch`                                 |
| Virtualized `node:fs` stub                                  | `@iso4/fs`                                    |
| Virtualized `node:crypto` stub                              | `@iso4/crypto`                                |
| Other Node stdlib stubs                                     | `@iso4/<name>`                                |
| Native binary distribution for @iso4/dynamic                | `@iso4/v8-<platform>`                         |
| Native binary distribution for @iso4/static                 | `@iso4/static-<platform>`                     |
| Worked-example apps                                         | `examples/<name>/`                            |
| Latency or throughput measurement                           | `bench/`                                      |

---

## 7. Public API surface, summarized

```ts
// Dynamic runtime — agent-generated code, crash-isolated
import { createRuntime } from '@iso4/dynamic'
import { createSafeFetch } from '@iso4/fetch'

// Static runtime — precompiled function + varying data (Phase 11)
import { createStaticRuntime } from '@iso4/static'

const runtime = await createRuntime()
const prefix = await runtime.precompile({
  code: `import { search } from 'tools:search'; globalThis.search = search;`,
  globals: { fetch: createSafeFetch({ allowedHosts: ['api.example.com'] }) },
  imports: { static: { 'tools:search': { kind: 'host', exports: { search } } } },
})
const result = await prefix.run({
  code: `export default await (${agentFn})()`,
})

const runtime = await createStaticRuntime({ maxConcurrent: 8 })
const prefix = await runtime.precompile({
  code: `export function transform(row) { return row.price * row.qty; }`,
})
const results = await Promise.all(
  rows.map((row) => prefix.call('transform', row))
)
```
