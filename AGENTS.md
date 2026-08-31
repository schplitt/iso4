# AGENTS.md

## Project Overview

**iso4** is a fast, sandboxed V8 isolate runtime with permission-controlled
`fetch` and pluggable modules. Built primarily for the AI-agent prefix/postfix
pattern: host code prepares setup (globals, data, tool bindings) once, then
runs many agent-generated postfixes against it. The prefix is stored as
validated source and evaluated per isolate — there is no runtime V8 snapshot
(DESIGN.md §11.6) — and prefix runs are served by resident warm instances
that skip that evaluation on reuse (DESIGN.md §13.2.1).

The project is a pnpm monorepo. Architecture and rationale are documented
exhaustively in three files:

- **`DESIGN.md`** — architecture, execution model, limits, security model,
  phased build plan. The design contract. Read this first for any non-trivial
  change, and before adding code to a new package or moving code between
  packages.
- **`docs/protocol.md`** — the canonical wire reference: frame table, payload
  layouts, error codes, versioning. Authoritative for anything on the socket.
- **`packages/iso4-sandbox/src/types.ts`** — canonical public API surface for
  `@iso4/sandbox`. Changes here are API changes and must align with DESIGN.md.

## Architecture

```
iso4/                              ← workspace root
  DESIGN.md                        ← design contract
  docs/protocol.md                 ← wire protocol reference
  package.json                     ← workspace root (private)
  pnpm-workspace.yaml              ← packages glob + catalog
  eslint.config.js                 ← @schplitt/eslint-config
  scripts/build-native.ts          ← cargo build + copy binary into the platform package
  .github/workflows/               ← CI + release
  packages/
    iso4-sandbox/                  ← @iso4/sandbox (the runtime)
      src/
        index.ts                   ← public re-exports + createSandbox
        types.ts                   ← canonical API types
        client.ts, pool.ts, ipc.ts ← IPC client, connection pool, frame codec
      tests/
      bench/
      __snapshots__/tsnapi/        ← public API snapshot (tsdown --update-snapshot)
      package.json
      tsconfig.json                ← standalone (no extends), copied from starter template
      tsdown.config.ts
    iso4-fetch/                    ← @iso4/fetch
    iso4-v8-<platform>/            ← native Rust binaries (built in CI, not committed)
  native/
    v8-runtime/                    ← Rust source for the V8 host binary
      src/
        main.rs                    ← arg parsing, UDS accept loop
        session.rs                 ← per-connection message loop, prefix store
        v8.rs                      ← isolates, contexts, prefix evaluation, limits
        warm.rs                    ← warm-instance registry
        policy.rs, rss.rs          ← eviction/pressure decisions + RSS reader
        ipc.rs, wire.rs, blob.rs   ← frames, payload codecs, V8 serialization
```

### `packages/iso4-sandbox`

The two-process dynamic runtime package (`@iso4/sandbox`). Owns:

- Public API: `createSandbox`, `Runtime`, `PrecompiledPrefix`, the type
  system declared in `src/types.ts`.
- IPC client + binary frame codec talking to the Rust V8 process.
- Bridge dispatch routing `_hostCall` frames to host-supplied handlers for
  configured globals/functions and imports.
- Mechanical fetch hygiene when the host chooses to expose `fetch`:
  header/URL/method validation, body size cap, no-auto-redirect default.
  Anything that is not policy.
- Runtime-owned sandbox shims where explicitly designed. `console.*` capture
  is one of them and it ships as native V8 callbacks in `v8.rs`
  (`install_console`), buffered per call and returned with the run's result —
  never an inline JS prelude.

Does **not** own:

- Any HTTP client. `fetch` is not special-cased as an always-present runtime
  global; hosts expose it through the bridge when needed.
- Any policy. Allow/deny decisions live in handlers the host supplies.
- Any Node-stdlib emulation. Those live in sibling `@iso4/<name>` packages.

### Sibling packages

`@iso4/fetch` (shipped) and the planned `@iso4/fs`, `@iso4/crypto`, … are
tiny factory packages that produce `FetchHandler` or host-import values ready
to plug into `createSandbox({ globals, imports })`. They hold policy and
capability surface; `@iso4/sandbox` holds none. A new one is added by the
same rule: it may depend on `@iso4/sandbox`'s public types only, never on its
internals.

## Development

```sh
pnpm install           # bootstrap workspace
pnpm build:dev         # build native binary (debug) + all TS packages — use for local dev
pnpm build             # build native binary (release) + all TS packages — use for CI / release
pnpm test:run          # cargo test (Rust unit tests) + vitest once across all TS packages
pnpm test              # vitest in watch mode (developer use only, TS only)
pnpm lint              # eslint
pnpm lint:fix          # eslint --fix
pnpm typecheck         # tsc --noEmit across packages
pnpm changeset         # record a per-package version bump (use on every user-facing change)
pnpm build:native      # build + copy release binary only (no TS build)
pnpm build:native:dev  # build + copy debug binary only (no TS build)
pnpm test:native       # cargo test only (Rust unit tests, no TS)
```

**Important:** The TS integration tests (`integration.test.ts`, `e2e.test.ts`) spawn
the native binary from the platform package (`packages/iso4-v8-<platform>/bin/iso4-v8`).
That binary must be built and copied before running TS tests. `pnpm build:dev` (or
`pnpm build`) does this. Running `pnpm test:run` without building first will use
whatever binary was last installed/built.

**Test baseline:** `pnpm test:run` should report **0 failures**. Run it for the
live pass/skip counts — don't rely on a number recorded here, it goes stale as
PRs land. The remaining skips cover features not yet implemented and are tracked
against the phase roadmap in `DESIGN.md`. Any new failure is a regression; the
skipped count should only drop as later phases land.

Toolchain:

- **pnpm** for package management (catalog + workspace protocol).
- **TypeScript** with strict config. Each package has its own standalone
  `tsconfig.json` copied from the starter template — no `extends`, no
  project references. Cross-package type resolution happens via pnpm's
  workspace symlinks in `node_modules`, not via TS project references.
- **tsdown** for builds; packages emit ESM only.
- **vitest** for tests, per package.
- **eslint** via `@schplitt/eslint-config`.
- **@changesets/cli** for per-package versioning + changelogs. See
  `.changeset/README.md` for the workflow.
- **mise** pins the Node version (`mise.toml`).
- **cargo** (Rust stable) for the V8 host binary under `native/v8-runtime/`.

## Code Style

- ESM only (`"type": "module"` everywhere).
- TypeScript strict mode enabled, plus extras: `exactOptionalPropertyTypes`,
  `noUncheckedIndexedAccess`, `noPropertyAccessFromIndexSignature: false`,
  `isolatedDeclarations`, `verbatimModuleSyntax`.
- Build via `tsdown` (zero-config-ish).
- Lint via `@schplitt/eslint-config` (the eslint.config.js is one-line).

## Testing

- Tests live in `packages/<pkg>/tests/`, named `*.test.ts`.
- Use `pnpm test:run` (single shot, no watch) in any automated path.
- Use `pnpm test` only for interactive development.
- Import from `../src` (relative) inside a package's tests.
- Cross-package integration tests (runtime ↔ Rust binary) will live in a
  top-level `bench/` or `examples/` once the runtime is wired up; not
  required during the scaffolding phase.

## Maintaining Documentation

When making changes to the project:

- **`DESIGN.md`** — Update whenever architecture, the execution model,
  resource-limit semantics, the security model, or the API surface changes.
  This is the design contract. Out-of-date design docs cost more than the
  code they describe. Also update it when a package is added, dependency
  direction shifts, or versioning policy changes.
- **`docs/protocol.md`** — Update whenever a frame, payload field, error
  code, or handshake rule changes. It is authoritative for the wire; where it
  and `DESIGN.md` disagree about frames, `DESIGN.md` is the one that is stale.
- **`packages/iso4-sandbox/src/types.ts`** — Update whenever the `@iso4/sandbox`
  API changes. Any deviation between this and `DESIGN.md` is a bug. JSDoc
  here is the published API documentation, so it must not describe removed
  behavior; changing it updates `__snapshots__/tsnapi/` (run
  `npx tsdown --update-snapshot` in the package and commit the result).
- **`AGENTS.md`** (this file) — Update with technical details, architecture,
  and best practices for AI agents.
- **`README.md`** — Update with user-facing documentation for end users:
  - ✅ New exported utilities or functions from any package.
  - ✅ New configuration options.
  - ✅ Changes to existing API behavior.
  - ✅ Installation or setup instructions.
  - ✅ Usage examples that should appear on first read.
- **Per-package `README.md`** — Update when that package's specific API or
  setup changes. The root README is for project orientation; per-package
  READMEs are for users of that package specifically.

## Agent Guidelines

When working on this project:

1. **Read the design docs first.** Almost every non-trivial change touches
   a decision already recorded in `DESIGN.md` or `docs/protocol.md`; reading
   the relevant section is cheaper than re-deriving it.
2. **Keep `types.ts` and `DESIGN.md` in sync.** If you change the API, both
   files change in the same commit. If you cannot match them, ask before
   committing.
3. **Run tests** after making changes: `pnpm test:run` (single-shot).
4. **Run linting**: `pnpm lint` (or `pnpm lint:fix`).
5. **Run type checking** before committing: `pnpm typecheck`.
6. **Validation order**: `pnpm lint:fix` → `pnpm typecheck` → `pnpm build:native:dev` → `pnpm test:run`. The native build step is required so the TS integration tests use the current binary. Skip it only when the Rust source has not changed.
7. **Public API stays in `packages/<pkg>/src/index.ts`.** All re-exports
   listed explicitly. No barrel-exports of `*`.
8. **Add tests** for new functionality in `packages/<pkg>/tests/`.
9. **Record learnings** — When the user corrects a mistake or provides
   context about how something should be done, add it to the "Project
   Context & Learnings" section below if it's a recurring pattern (not a
   one-time fix).
10. **Notify documentation changes** — When updating `README.md`,
    call out the changes to the user at the end of your response so they
    can review and don't overlook them.
11. **Keep protocol docs in sync with code** — Whenever a new `RunError`
    variant or `ERR_*` code is added, all four of the following must be
    updated in the same commit:
    - `RunError` enum in `v8.rs` (+ `run_error_to_payload` in `wire.rs`)
    - `RunErrorCode` union in `packages/iso4-sandbox/src/types.ts`
    - Error-code table in `docs/protocol.md` §7
    - Any relevant limits/semantics prose in `DESIGN.md` §4.1 (for limit
      fields) or the appropriate design section (for new concepts)
      Missing any of these is a documentation bug equivalent to a broken
      API contract.
12. **Use available workflow tools first** — When the user asks for
    branch/commit/PR workflow, use the available MCP/devtools first. Only
    fall back to `gh` CLI when those tools are not available.
13. **Use conventional naming for git workflow** — Branch names should use
    conventional prefixes where appropriate: `feat/`, `fix/`, `chore/`,
    `docs/`, `refactor/`, `test/`, `build/`, `types/`, `style/`, `perf/`,
    `examples/`, `ci/`. Commit subjects and PR titles should use
    conventional-commit style.
14. **Default PR behavior** — If the current branch already contains the
    related work, assume the PR should be opened from the current branch
    to `main` unless the user explicitly asks otherwise.
15. **Always include a PR body** — PRs created for the user must include a
    body. If a related issue identifier is known, include the appropriate
    GitHub-style reference.
16. **Ask when requirements are unclear** — If requirements are ambiguous,
    ask a focused clarifying question instead of implementing a guessed
    solution. This project is in early design; many decisions are still
    open and the wrong choice is more expensive than a clarifying round trip.
17. **Prefer simple inline logic over trivial helpers** — Do not introduce
    tiny one-line helper/utility functions or throwaway `parse*` helpers
    for trivial one-off logic. Inline unless there is real reuse or a clear
    API boundary.
18. **Respect package boundaries** — Before adding code, check the "Owns" /
    "Does not own" lists above and the layout in `DESIGN.md` §8. If the new
    code does not fit an existing row, the design needs a conversation before
    the code does.

## Project Context & Learnings

This section captures project-specific knowledge, tool quirks, and lessons
learned during development.

### Tools & Dependencies

- Use `pnpm test:run` in automated/agent workflows. `pnpm test` starts watch
  mode and will hang automation.
- Run `pnpm build:native:dev` before `pnpm test:run` whenever Rust source
  has changed; the TS integration tests spawn the binary from the platform
  package directory and will use a stale binary otherwise.
- `pnpm test:run` now includes `pnpm test:native` (Rust unit tests via
  `cargo test`) before the TS vitest run. The Rust unit tests use the debug
  profile and are fast; no separate `cargo test` call is needed.
- Prefer `pnpm lint:fix` before manual lint cleanup.
- The pnpm catalog (`pnpm-workspace.yaml`) is the single source of truth
  for dev-dep versions. Reference deps as `"catalog:"` in package.json,
  not hard-coded versions.
- Node 24+ is required; `mise.toml` pins to 26 for development. CI uses 26.
- `spawn_responder` in `v8.rs` tests reads and discards one frame before calling the `respond` closure. Tests using `run_with_bridge` where the respond closure needs to handle multiple calls must NOT try to read frames inside respond — use `drain_bridge_calls` with a direct socket pair instead (see `bridge_call_exactly_at_limit_succeeds` as the pattern to follow).
- The TS test suite should be green with zero failing tests. Run
  `pnpm test:run` for the live pass/skip counts — do not trust a hard-coded
  number here, it goes stale as PRs land. Any failure after a change is a
  regression. Remaining skips cover features not yet implemented and are
  unskipped as the corresponding phases land.

### Patterns & Conventions

- ESM only. No CJS anywhere.
- Use conventional branch prefixes and conventional-commit style commit
  subjects / PR titles.
- The public API of every package is what's exported from its
  `src/index.ts`. Tests import from `../src/index.js` (note the `.js`
  extension — required by NodeNext-style ESM resolution).
- Keep trivial one-off normalization and branching inline instead of
  extracting tiny helpers too early.
- If requirements are ambiguous, ask a focused clarifying question first.

### iso4-specific architectural rules

These are recurring decisions; deviating from them needs an explicit
conversation. Codified in `DESIGN.md` but worth keeping front-of-mind:

- **Only data crosses the sandbox boundary.** Functions never cross by
  value. V8 `ValueSerializer` enforces this naturally on exports; the
  bridge enforces it explicitly on host import calls. Note that "data" is
  everything V8's format carries — `Date`, `Map`, `Set`, `RegExp`, `Error`,
  typed arrays, cycles — not just JSON-shaped values.
- **User code is always ESM.** Results come back via `export default` /
  named `export`s, never via globals or `console`.
- **Globals are runtime-curated, block-listed not allowlisted.** The host may
  contribute any name that is not on the reserved list (JS built-ins plus the
  runtime's own web globals and `console`); `fetch` is not special-cased.
  Prefer `imports` for anything user code does not expect as a bare name.
  Changing what the runtime installs or reserves is a deliberate design
  decision recorded in `DESIGN.md` §4.2 / §4.2.1.
- **`iso4` has no HTTP client and no network policy.** Mechanical hygiene
  (header validation, URL parsing, body cap) lives in `iso4`. Real fetch
  behavior + policy lives in `@iso4/fetch` or in host code.
- **No callbacks across the boundary** in v1. No `setTimeout`, no
  event listeners on host objects. Functions passed as host-import
  arguments are rejected with `ERR_FUNCTION_ARGUMENT_NOT_SUPPORTED`.
- **One execution model in this product.** `@iso4/sandbox` ships one API
  shape: independent runs against a prepared prefix (`prefix.execute()` /
  `prefix.call()`, plus one-off `sandbox.run()`). Warm instances make those
  runs cheap but are a transparent cache, not a second model — no session
  handle, no host-visible lifecycle. The persistent-session
  / per-row-analytics use case is its own product (`@iso4/analytics` or
  similar, name TBD), not a `SandboxOptions.backend` flag. They will
  share wire-frame types, the `Imports<…>` declaration shape, and the
  `ValueSerializer`-based "only data crosses" rule, but the runtime API,
  transport, lifecycle, and memory model differ. Do not retrofit a
  session API or an in-process backend onto `@iso4/sandbox`. See
  DESIGN.md §9.1 and §13.
- **Two-process is the only backend.** The Rust subprocess provides
  crash isolation critical for untrusted code. There is no
  `SandboxOptions.backend` flag and no plan to add one in this package.
  In-process embedding is the analytics product's concern.
- **V8 `ValueSerializer` is irreplaceable for JS values.** Cap'n Proto,
  MessagePack, and similar cannot replace it for bridge/export payloads
  because they don't know V8's type system. They can only wrap V8 bytes as
  opaque `Data` blobs — net overhead, not savings. The current
  length-prefixed frame format is already appropriately minimal.

### Common Mistakes to Avoid

- Do not use `pnpm test` in automation.
- Do not create tiny helper/utility functions or `parse*`/`normalize*`
  wrappers for trivial one-off logic.
- Do not guess when the requested behavior or scope is unclear.
- Do not import from a sibling package's internals. Use its public
  `package.json` exports. If something needs to be shared between sibling
  packages, that's a design discussion (not just a refactor).
- Do not silently install new top-level globals into a prepared prefix's
  context at run time — that breaks the invariant that the declared prefix
  shape is the full surface the prefix was validated against. Surface with
  `ERR_UNDECLARED_BINDING` instead.
- Do not add a default HTTP client to `iso4`. Sandbox network access
  must be deny-by-default.
- Do not add ad-hoc inline JS preludes for `console`, `crypto`, or `fetch`.
  Runtime-owned surface is installed natively (`install_console`,
  `webtypes::install`), and `fetch` is just a host-configured bridge
  global/function rather than a special always-installed runtime stub.
- Do not forget to run `pnpm changeset` when a PR changes user-visible
  behavior in a published package. Internal-only changes (refactors,
  tests, docs) do not need one.
- Do not write a long changeset. It becomes the package's `CHANGELOG.md`
  entry and the body of its GitHub release, so it is release notes rather
  than a commit message: one conventional-commit line,
  then at most two sentences covering what a reader will notice or has to
  do differently. Rationale, mechanism, measurements and file names go in
  the commit message and the PR. See `.changeset/README.md`.
- Do not add Cap'n Proto, MessagePack, or similar as the IPC serialization
  format. V8 `ValueSerializer` is the only correct format for JS values
  crossing the sandbox boundary. The wire envelope (4-byte length + 1-byte
  type) is already minimal; don't wrap it in a schema library.
- Do not use `num-bigint` (or any other arbitrary-precision crate) for BigInt
  handling. V8's serialization format carries BigInts natively in both
  directions, so no base conversion — and no crate — is needed anywhere.
- Do not add an in-process (NAPI) backend to `@iso4/sandbox`. It belongs
  to the future analytics product (DESIGN.md §9.1), not as a
  `SandboxOptions.backend` flag. rusty_v8 cannot be used in-process with
  Node and would need a separate C++ embedding. That conversation
  happens in the analytics product's design pass, after Phases 1–13 of
  this product ship.
