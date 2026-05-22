# iso4 — Monorepo layout

The reason this is a monorepo and not a single package: the **runtime is
small, opinionated, and rarely changes**. Everything composed on top of it
— hardened fetch, stdlib stub factories (`@iso4/fs`, `@iso4/crypto`, …),
tool-calling helpers, agent integrations — has its own release cadence,
its own dependency set, and its own surface area. Bundling them all into
one package would force every consumer to pull in deps they don't use and
would couple unrelated release cycles.

The rule we follow: **a package exists when it has a different consumer or
a different set of trust assumptions than the runtime.**

Naming convention:
- **`iso4`** — the main runtime package, unscoped, the one you `npm i` to
  get started. Exports `createRuntime`, types, the API surface.
- **`@iso4/<name>`** — every extension. Hardened fetch (`@iso4/fetch`),
  stdlib stub factories (`@iso4/fs`, `@iso4/crypto`, …), platform-specific
  binaries (`@iso4/v8-darwin-arm64`, …). The scope makes ownership obvious
  and reserves the namespace.

---

## 1. Package map

```
iso4/
  DESIGN.md
  MONOREPO.md                       ← this file
  README.md
  README.md
  package.json                      ← workspace root, no publishable contents
  pnpm-workspace.yaml
  turbo.json                        ← (or biome/justfile — TBD)
  .changeset/                       ← per-package version bumps
  packages/
    iso4/                           ← iso4                       (runtime, types, createRuntime)
    iso4-fetch/                     ← @iso4/fetch                (hardened FetchHandler)
    iso4-fs/                        ← @iso4/fs                   (future, node:fs stub factory)
    iso4-crypto/                    ← @iso4/crypto               (future, node:crypto stub factory)
    iso4-v8-darwin-arm64/           ← @iso4/v8-darwin-arm64      (native binary)
    iso4-v8-darwin-x64/             ← @iso4/v8-darwin-x64
    iso4-v8-linux-x64-gnu/          ← @iso4/v8-linux-x64-gnu
    iso4-v8-linux-arm64-gnu/        ← @iso4/v8-linux-arm64-gnu
  native/
    v8-runtime/                     ← Rust source for the V8 host binary
      Cargo.toml
      src/
        main.rs
        isolate.rs
        session.rs
        execution.rs
        bridge.rs
        ipc.rs
        budget.rs
        timeout.rs
        snapshot.rs
  examples/
    minimal/                        ← runtime.run, no fetch, no imports
    agent-prefix-postfix/           ← canonical AI loop with precompile
    safe-fetch/                     ← @iso4/fetch in action
    fs-stub/                        ← @iso4/fs in action
  bench/                            ← startup latency, throughput benchmarks
```

---

## 2. Packages

### 2.1 `iso4` — the runtime

The only package every consumer needs. Unscoped on purpose: it's the
entry point you `npm i` to get started, and the name is short.

Owns:

- The TypeScript host API: `createRuntime`, `Runtime`, `PrecompiledPrefix`,
  the type system declared in `src/types.ts` (the canonical API surface).
  The repo root used to hold a `types.ts` spec next to `DESIGN.md`; that
  file now lives in this package.
- Spawning the per-platform Rust binary (via `optionalDependencies` on the
  `@iso4/v8-*` packages, mirroring secure-exec's distribution).
- The IPC client, frame codec, V8 ValueSerializer wrapper.
- The bridge dispatch: routing `_hostCall` frames to host-supplied handlers
  for `fetch` and `imports`.
- **Mechanical fetch hygiene**: header/URL/method validation, response
  size cap, no-auto-redirect default. Anything that isn't policy.
- The sandbox-side JS shims (`console`, `crypto.getRandomValues`, the
  `fetch` stub + `Response` polyfill, module proxy generation).

Dependencies: minimal. Just what's needed for UDS I/O and basic process
management. No HTTP client.

What this package **does not** ship:
- Any HTTP client. If sandbox code calls `fetch` without a configured
  handler, the run fails with `ERR_FETCH_NOT_CONFIGURED`. There is no
  default that silently uses the host's network.
- Any policy. Allow/deny decisions live in handlers the host supplies.
- Any Node-stdlib emulation. `iso4` knows nothing about `node:fs`,
  `node:crypto`, etc. Those are entirely separate packages.

### 2.2 `@iso4/fetch` — hardened fetch defaults

Optional but strongly recommended. Owns:

- `createSafeFetch(options)` returning a `FetchHandler` ready to plug into
  `globals.fetch`.
- DNS pre-resolution with IP pinning (prevents DNS rebinding).
- Allowlist/denylist matching (exact host, suffix, regex).
- Private/link-local IP blocking (RFC1918, loopback, link-local, ULA).
- Redirect re-checking (each hop runs the allowlist again, configurable
  max redirects).
- Per-host timeout and concurrency caps.
- An isolated `undici` Dispatcher so sandbox traffic doesn't share state
  with the host application's HTTP pool, auth cookies, etc.

Dependencies: `undici` (or `node:fetch` shim under non-Node hosts).

Reason for being separate:
- `undici` is heavy. Hosts that ship their own HTTP client shouldn't pay
  for it.
- The security surface here (DNS, redirect, allowlist matching) is its own
  domain with its own bugs and its own audit cycle. Decoupling release
  cadence from the core runtime matters.
- Some hosts will write entirely custom fetch handlers (e.g., a CDN
  origin-fetch with internal auth). They should be able to ignore
  `@iso4/fetch` entirely.

### 2.3 `@iso4/v8-<platform>` — native binaries

One package per supported platform. Each contains a single compiled Rust
binary plus a tiny `index.js` that exports the binary path. `iso4` lists
these as `optionalDependencies`; pnpm/npm install only the one matching
the host's `process.platform + process.arch`.

Initial platforms:
- `@iso4/v8-darwin-arm64`
- `@iso4/v8-darwin-x64`
- `@iso4/v8-linux-x64-gnu`
- `@iso4/v8-linux-arm64-gnu`

Add `@iso4/v8-win32-x64` when there's demand.

No source code in these packages — they're build artifacts. Source lives
under `native/v8-runtime/` and is built once per release per platform via
CI.

### 2.4 Stdlib-stub packages (`@iso4/fs`, `@iso4/crypto`, …)

The single most useful kind of extension package. Each one is a tiny
factory that produces a `HostImport` matching the shape of a Node.js
stdlib module, ready to plug into `imports.static`.

**Why this pattern exists:** the `iso4` runtime ships zero `node:*`
emulation. If a host author wants `import fs from "node:fs"` to work in
the sandbox, they have to provide it. Writing a correct, safe, virtualized
`node:fs` is tedious. Instead, install `@iso4/fs` and drop it in:

```ts
import { createRuntime } from "iso4";
import { createFsModule } from "@iso4/fs";

const runtime = await createRuntime();
const prefix = await runtime.precompile({
  code: prefixSource,
  imports: {
    static: {
      "node:fs": createFsModule({
        root: "/sandbox",
        readOnly: true,
        maxFileBytes: 1 * 1024 * 1024,
      }),
    },
  },
});
```

Each stdlib-stub package owns:
- A factory function that takes per-host configuration (root path,
  permissions, size limits, etc.).
- Returns a `HostImport` (`{ kind: "host", exports: { ... } }`).
- Validates inputs at the bridge boundary, just like core does for fetch.
- Has its own threat model documented in its README. `@iso4/fs` doesn't
  let sandbox code escape via path traversal even if the host author
  forgets to set `root`; the package's defaults are deny-by-default.

Planned (not in v1, in rough priority order):

- **`@iso4/fs`** — virtualized `node:fs` against a configurable host root
  with permission helpers (read-only mounts, denylist patterns, size
  caps).
- **`@iso4/crypto`** — the safe parts of `node:crypto`: hashing, HMAC,
  random, base64, hex. Anything involving the host keystore is excluded.
- **`@iso4/path`** — `node:path` is pure (no I/O); ships as a source
  module rather than a host module. Trivial, but worth packaging.
- **`@iso4/url`** — same idea, source-module shim for libraries that
  expect `node:url`.
- **`@iso4/buffer`** — `node:buffer` shim. Same source-module pattern.
  Most modern libs use `Uint8Array` natively, so this is for older code.

### 2.5 Other future packages (not in v1)

- **`@iso4/tools`** — helpers for the AI tool-calling pattern.
  `defineTool({ name, schema, handler })` produces a host module ready to
  plug into `imports`, with zod-like input validation. Separate because
  tool-protocol shapes are contested and will change.
- **`@iso4/console`** — alternate console implementations (structured
  logging, JSON-line capture, integration with pino/winston). The default
  runtime-owned console is intentionally minimal.
- **`@iso4/snapshot-cache`** — disk-backed snapshot persistence across
  host restarts. Snapshots are tens-to-hundreds of KB; for very large
  prefixes that's worth caching to disk. Not needed at v1 scale.
- **`@iso4/tracing`** — OpenTelemetry spans for every bridge call.
  Useful for production observability, opt-in.

The rule for adding any new package: **at least two consumers want it,
and the surface area is meaningfully different from anything that already
exists.**

---

## 3. Dependency direction

```
                       iso4
                  ▲  ▲  ▲  ▲
                  │  │  │  │
        @iso4/fetch  │  │  @iso4/crypto
                     │  │
               @iso4/fs  @iso4/tools (future)
                     │
                     ▼
              host application
```

Rules:

- **`iso4` depends on nothing in this monorepo** other than the
  per-platform `@iso4/v8-*` binaries (optional, runtime-resolved).
- **Every other package depends on `iso4`** (as a peer dependency),
  never the other way around.
- **Sibling packages (`@iso4/fetch`, `@iso4/fs`, etc.) do not depend
  on each other.** If they need to share a utility, it goes into the
  core `iso4` package's `internal` export (carefully, with a clear name).
- **No package depends on `examples/` or `bench/`.** Those are leaves.

Why this matters: if `@iso4/fs` ever depended on `@iso4/fetch`, a host
that wanted only the FS stub would pull in `undici` (a fetch dep)
transitively. That's the kind of incidental coupling that makes monorepos
rot.

---

## 4. Versioning

Independent per package, via [`@changesets/cli`](https://github.com/changesets/changesets).
Every user-visible PR adds a changeset file (`.changeset/<random>.md`)
declaring which packages bump and at what level. On push to `main`, the
`changesets/action` workflow opens a "Version Packages" PR; merging that
PR triggers publish.

Reasons:

- `iso4` changes infrequently after v1 and breaking changes are
  expensive (every consumer pays).
- `@iso4/fetch` and the stdlib-stub packages may release security
  patches more often.
- `@iso4/v8-*` packages bump when V8 itself bumps, independently of API
  changes.

What does NOT change in lockstep:
- Bumping `iso4` does not force-bump `@iso4/fetch` or any stdlib-stub
  package. Each pins its `iso4` peer dependency range explicitly.
- Bumping a `@iso4/v8-*` patch (new V8 version, same protocol) does not
  bump `iso4`.

What DOES change in lockstep:
- Wire-protocol changes between `iso4` (host) and `@iso4/v8-*` (Rust).
  Both packages bump major. The host validates the binary version on
  connect and refuses mismatched pairs with a clear error.

The protocol version is a `u16` in the `Authenticate` frame. Bump it
whenever the wire format changes incompatibly. This is the single mechanism
preventing "I installed iso4@2 but my @iso4/v8-darwin-arm64@1 binary is
still cached" foot-guns.

---

## 5. Build and dev workflow

Tooling targets:
- **pnpm** for package management (workspace protocol, peer auto-install).
- **TypeScript** with `composite: true` for cross-package type checking.
- **tsup** or **esbuild** for builds; the host packages bundle to dual
  ESM/CJS.
- **vitest** for tests, per package, plus an end-to-end suite in `bench/`
  that exercises real Rust binaries.
- **Cargo** for the Rust crate.
- **GitHub Actions** for per-platform native builds with `cross` or
  cross-compilation runners. Build matrix mirrors the `@iso4/v8-*` packages.

Typical commands (top-level):

```bash
pnpm install               # bootstrap
pnpm build                 # build all TS packages
cargo build --release      # build the Rust binary for the host platform
pnpm test                  # vitest across all packages
pnpm bench                 # latency / throughput benchmarks
pnpm changeset             # record a version bump
pnpm release               # build + version + publish
```

Native binary build per release: a CI matrix runs `cargo build --release
--target=<triple>` per platform and stages the artifact into the
corresponding `@iso4/v8-<platform>/` package's `npm/` folder. Cross-builds
use GitHub-hosted runners (macOS for Apple Silicon, Linux for the Linux
targets) — no fancy cross-compilation tricks needed for v1.

---

## 6. What goes where: a quick reference

When you find yourself wanting to add code, ask:

| If it's about… | Put it in |
|---|---|
| The V8 isolate, threads, snapshots, bridge dispatch | `native/v8-runtime/` |
| The wire protocol or IPC framing | `iso4` AND `native/v8-runtime/` (must match) |
| Validating that headers/URLs/methods are well-formed | `iso4` (mechanical hygiene, no opinions) |
| Deciding *which* URLs are allowed | `@iso4/fetch` (or host app code) |
| Sandbox-side JS shims (`console`, `fetch` stub, etc.) | `iso4` |
| Hardened HTTP client behavior (DNS pin, redirect re-check, etc.) | `@iso4/fetch` |
| Virtualized `node:fs` stub with permission checks | `@iso4/fs` |
| Virtualized `node:crypto` stub | `@iso4/crypto` |
| Other Node stdlib stubs (`node:path`, `node:url`, `node:buffer`) | `@iso4/<name>` |
| Tool-call schema, input validation, agent helpers | `@iso4/tools` (future) |
| Native binary distribution | `@iso4/v8-<platform>` |
| Worked-example apps | `examples/<name>/` |
| Latency or throughput measurement | `bench/` |

If the answer doesn't fit any row, the design needs a conversation before
the code does.

---

## 7. Public API surface, summarized

For a host author:

```ts
// Always:
import { createRuntime } from "iso4";

// Pick what you need:
import { createSafeFetch } from "@iso4/fetch";  // hardened fetch defaults
import { createFsModule } from "@iso4/fs";       // node:fs stub  (future)
import { createCryptoModule } from "@iso4/crypto"; // node:crypto stub (future)

const runtime = await createRuntime();

const prefix = await runtime.precompile({
  code: `
    function callTool(name, args) { return globalThis._tool(name, args); }
    globalThis.callTool = callTool;
  `,
  globals: {
    fetch: createSafeFetch({ allowedHosts: ["api.example.com"] }),
  },
  imports: {
    static: {
      "node:fs": createFsModule({ root: "/sandbox", readOnly: true }),
      "agent/tools": { kind: "host", exports: {
        _tool: async (name, args) => myToolRouter(name, args),
      }},
    },
  },
});

const result = await prefix.run({
  code: aiGeneratedCode,
  limits: { cpuTimeMs: 200 },
});
```

That entire surface is the goal. The host imports one package per
capability they want, and each package is a small factory function.
Anything more verbose than that means a feature has crept somewhere it
shouldn't have.
