# Native side of iso4

The Rust code behind `@iso4/sandbox`. It builds one binary, `iso4-v8`, which
the TypeScript package spawns and talks to over a Unix socket. The binary is
shipped inside the `@iso4/v8-<platform>` npm packages and is never published
to crates.io.

## The three crates

| Crate                 | What It Is                                                                                                                                                | Depends On      |
| --------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------- |
| [`types/`](./types)   | The plain Rust types every side agrees on: limits, the shape of a prefix, bridge records, error payloads. No V8, no sockets.                              | nothing         |
| [`core/`](./core)     | The engine. Isolates, limits, warm instances, the prefix store, the web globals, the value codec, the startup snapshot. It never sees a byte of the wire. | `types`         |
| [`server/`](./server) | The binary. Opens the socket, speaks the frame protocol in `docs/protocol.md`, and drives the engine. The only crate that knows how Node talks to us.     | `core`, `types` |

Why the split: the engine and the wire change for different reasons. A new
frontend (a pool supervisor, a standalone service, a different transport)
links `core` and never touches `server`. See `DESIGN.md` §8.

## Also here

- `vendor/v8/` (gitignored) is the pinned `v8` crate with `patches/`
  applied. `scripts/prepare-v8.ts` unpacks it; the workspace `Cargo.toml`
  points `[patch.crates-io]` at it. Bump the version in that script.
- `rust-toolchain.toml` pins Rust stable.
- `Cargo.toml` is the workspace root. Profiles and the `v8` patch live there,
  not in the member crates.

## Build and test

Always from the repo root, so the scripts can prepare `vendor/` first:

```sh
pnpm build:native:dev    # debug binary, copied into the platform package
pnpm build:native        # release binary
pnpm test:native         # cargo test for all three crates
pnpm check:native        # cargo check --all-targets
pnpm lint:native         # clippy with warnings denied
pnpm fmt:native          # rustfmt
pnpm bench:native        # criterion micro benches (core)
```

Plain `cargo` works too once `pnpm prepare:v8` has run:

```sh
cargo build --manifest-path crates/Cargo.toml --package iso4-server
```

The first build downloads a prebuilt `librusty_v8.a` (about 100 MB). On
Linux, `core/build.rs` needs `clang++` to compile `clock_platform.cc`
against the vendored V8 headers.

## Diagnostics

The binary logs to stderr, which `@iso4/sandbox` forwards. Handshake
failures, protocol violations, and prefix lifecycle events are always
logged. Per-run trace lines are off by default because they cost 2 to 4 %
on a hot `prefix.call()`; turn them on with `ISO4_V8_TRACE=1` in the
environment the host spawns the binary from.

## License

MIT
