# iso4-v8-runtime

Rust source for the V8 host binary that `@iso4/sandbox` spawns to run
sandboxed JavaScript. The compiled binary is shipped to users via the
per-platform `@iso4/v8-<platform>` npm packages — it is **not** published
to crates.io.

> **Status:** in progress.

## What this binary is

A single binary (`iso4-v8`) that:

1. Binds a Unix domain socket in a `0700` tmpdir.
2. Authenticates the host process via a 128-bit token passed as a CLI arg.
3. Per `Run` message: spawns a session thread, creates a `v8::Isolate`
   with the configured `heap_limits`, compiles user code as an ESM
   module, evaluates it, captures `console` + exports, sends the result
   back over the socket.
4. Enforces memory and CPU limits via V8 callbacks and a dedicated
   timeout thread.

The host TypeScript package (`@iso4/sandbox`) never touches V8 directly —
it only sends frames. This binary owns all V8 state.

For the full architectural reasoning (why out-of-process, why one OS
thread per isolate, how snapshots work, how the CPU budget excludes
async-time, etc.) read [`../../DESIGN.md`](../../DESIGN.md).

## Build

Requires Rust stable (pinned via [`rust-toolchain.toml`](./rust-toolchain.toml)).

```sh
# From the repo root — preferred (copies the binary into the platform package):
pnpm build:native:dev    # debug build
pnpm build:native        # release build

# Or directly with cargo:
cargo build              # debug
cargo build --release    # release
```

The `v8` crate downloads a prebuilt `libv8_monolith.a` on first build
(~100 MB). Subsequent builds use the cached artifact.

## Lint & format

```sh
cargo fmt --all              # apply formatting
cargo fmt --all -- --check   # check formatting
cargo clippy --all-targets -- -D warnings
cargo check --all-targets
```

These run as part of `ci.yml` on every push and PR.

## Distribution

The binary is **not** published to crates.io. It is a private implementation
detail of the `@iso4/sandbox` npm package. Release binaries are built by the
`release.yml` CI workflow via a matrix across all supported platforms, then
placed into the corresponding `@iso4/v8-<platform>` npm packages before
`changeset publish` runs.

Platform packages: `@iso4/v8-darwin-arm64`, `@iso4/v8-darwin-x64`,
`@iso4/v8-linux-x64-gnu`, `@iso4/v8-linux-arm64-gnu`.

Platform packages: `@iso4/v8-darwin-arm64`, `@iso4/v8-darwin-x64`, `@iso4/v8-linux-x64-gnu`, `@iso4/v8-linux-arm64-gnu`.

## Wire protocol versioning

The IPC framing in `src/ipc.rs` must stay byte-for-byte identical with the
TypeScript codec in `packages/iso4-sandbox/src/ipc.ts`. Both sides include a
`u16` protocol version in the `Authenticate` frame; the host refuses to
connect to a binary reporting a different version. When the wire format
changes incompatibly, `@iso4/sandbox` and every `@iso4/v8-*` package bump
major together.

## License

MIT
