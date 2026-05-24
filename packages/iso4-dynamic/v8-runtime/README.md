# iso4-v8-runtime

Rust source for the V8 host binary that the `iso4` package spawns to run
sandboxed JavaScript. The compiled binary is shipped to users via the
per-platform `@iso4/v8-<platform>` npm packages.

> **Status:** scaffolding. `main()` is a not-implemented stub. The real
> implementation lands per the phased build plan in
> [`../../DESIGN.md`](../../DESIGN.md) §9.

## What this crate is

A single binary (`iso4-v8`) that:

1. Binds a Unix domain socket in a `0700` tmpdir.
2. Authenticates the host process via a 128-bit token (passed by env).
3. Per `Run` message: spawns a session thread, creates a `v8::Isolate`
   with the configured `heap_limits`, compiles user code as an ESM
   module, evaluates it, captures `console` + exports, sends the result
   back over the socket.
4. Enforces memory and CPU limits via V8 callbacks and a dedicated
   timeout thread.

The host TypeScript package (`iso4`) never touches V8 directly — it only
sends frames. This crate owns all V8 state.

For the full architectural reasoning (why out-of-process, why one OS
thread per isolate, how snapshots work, how the CPU budget excludes
async-time, etc.) read [`../../DESIGN.md`](../../DESIGN.md).

## Build

Requires Rust stable (pinned via [`rust-toolchain.toml`](./rust-toolchain.toml)).

```sh
# Debug build (fast, good for local dev)
cargo build

# Release build (used by the @iso4/v8-<platform> packaging)
cargo build --release
```

The `v8` crate downloads a prebuilt `libv8_monolith.a` on first build
(~100 MB). Subsequent builds use the cached artifact.

## Lint & format

```sh
cargo fmt --all              # apply formatting
cargo fmt --all -- --check   # CI: check formatting
cargo clippy --all-targets -- -D warnings
cargo check --all-targets
```

CI runs the latter three on every PR that touches `native/**`. See
[`../../.github/workflows/ci-rust.yml`](../../.github/workflows/ci-rust.yml).

## Planned module layout

To be created as each phase of the build plan lands. See
[`../../DESIGN.md`](../../DESIGN.md) §8 for the canonical map and §9 for
the phasing.

```
src/
  main.rs       UDS accept loop, signal handling, connection lifecycle
  ipc.rs        Binary frame codec (must match packages/iso4/src/ipc.ts)
  isolate.rs    V8 init, heap_limits, ArrayBuffer allocator, near-OOM cb
  session.rs    Per-run thread, concurrency slot, event loop
  execution.rs  Compile + evaluate ESM, module resolver, export extraction
  bridge.rs    FunctionTemplate stubs, sync/async bridge dispatch
  budget.rs     CPU budget enter/leave bracketing (async-time exclusion)
  timeout.rs    Wall-clock backstop thread → terminate_execution()
  snapshot.rs   v8::StartupData create + restore for precompiled prefixes
```

## How the host consumes the binary

At runtime, `iso4` looks for the binary in this order:

1. `iso4-v8-runtime/target/release/iso4-v8` (workspace dev path)
2. `iso4-v8-runtime/target/debug/iso4-v8` (workspace dev path)
3. `@iso4/v8-<platform>/iso4-v8` (the published per-platform npm package)
4. On `$PATH` (last-ditch fallback)

The platform npm packages (`@iso4/v8-darwin-arm64`, `@iso4/v8-linux-x64-gnu`,
etc.) each contain a single compiled binary plus a tiny `index.js`. CI
matrix builds them once per release via `cargo build --release --target=<triple>`.

See [`../../MONOREPO.md`](../../MONOREPO.md) §2.3 for the distribution
model.

## Wire protocol versioning

The IPC framing in `src/ipc.rs` (when added) MUST stay byte-for-byte
identical with the TypeScript codec in `packages/iso4/src/ipc.ts`. Both
sides include a `u16` protocol version in the `Authenticate` frame; the
host refuses to connect to a binary reporting a different version. When
the wire format changes incompatibly, `iso4` and every `@iso4/v8-*`
package bump major together. See [`../../MONOREPO.md`](../../MONOREPO.md)
§4.

## License

MIT
