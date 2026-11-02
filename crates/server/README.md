# iso4-server

The `iso4-v8` binary: the engine served over a Unix socket to the Node host.
This is the only crate that knows the wire.

## What it does

1. `main.rs` parses the flags (`--socket`, `--warm-budget-bytes`,
   `--host-reserve-bytes`, `--idle-settle-secs`), prints one capacity line,
   creates one `Engine`, and accepts connections.
2. `session.rs` runs each connection: the `Authenticate` handshake, then the
   demux loop. It is the only reader on the socket. It turns frames into
   engine requests and run messages, and the connection's `HostLink`
   implementation is the one serialized writer, so frames from concurrent
   runs never interleave.
3. `ipc.rs` owns the frame envelope, the outbox, and the payload parsers;
   `wire.rs` owns the encoders for everything the engine sends back.

The layout of every frame and payload is in `docs/protocol.md`. The
TypeScript side is `packages/iso4-sandbox/src/ipc.ts`; the two must stay
byte-for-byte compatible, and both carry the protocol version in the
handshake.

## What it does not do

It runs no JavaScript and enforces no limit. All of that is `iso4-core`,
reached through `Engine`, `HostLink`, `Outbound` and `Inbound`. A different
host, in any language, talks to this same binary by speaking the same wire;
a different transport would be a sibling crate linking `core`.

## Build and run

From the repo root, `pnpm build:native:dev` builds the debug binary and
copies it into the platform package the TypeScript tests spawn. Directly:

```sh
cargo build --manifest-path crates/Cargo.toml --package iso4-server
crates/target/debug/iso4-v8 --socket /path/in/a/private/dir.sock
```

The host hands the binary a fresh socket path inside a directory only it
can read; access control is that directory, not a token.

## Tests

`cargo test --manifest-path crates/Cargo.toml -p iso4-server` covers the
handshake, frame framing, payload parsers and encoders, and the demux
against a real engine over a socket pair. The end-to-end behavior is
covered by the TypeScript suites in `packages/iso4-sandbox/tests`, which
spawn this binary.
