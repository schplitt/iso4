# iso4-server

The `iso4-v8` binary: the engine served over a Unix socket to the Node host.
This is the only crate that knows the wire.

## What it does

1. `main.rs` parses the flags (`--socket`, `--warm-budget-bytes`,
   `--host-reserve-bytes`, `--idle-settle-secs`, `--cores`, `--node-pid`),
   writes the pid file, prints one capacity line, creates one `Engine`, and
   accepts connections.
2. `session.rs` runs each connection: the `Authenticate` handshake, then the
   demux loop. It is the only reader on the socket. It turns frames into
   engine requests and run messages, and the connection's `HostLink`
   implementation is the one serialized writer, so frames from concurrent
   runs never interleave.
3. `ipc.rs` owns the frame envelope, the outbox, and the payload parsers;
   `wire.rs` owns the encoders for everything the engine sends back.
4. `process.rs` writes the pid file and runs the `--node-pid` watch.

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

## Flags

- `--socket <path>` (required): where to listen.
- `--warm-budget-bytes`, `--host-reserve-bytes`, `--idle-settle-secs`: the
  memory budget, the Node host's reserve, and the idle settle delay.
- `--cores <n>` (n >= 1): use n in place of the machine's core count for the
  V8 worker pool (n - 1 threads, at least 1, at most 16), the per-prefix
  instance ceiling, and the cold-start slot allowance. Absent: the machine's
  count.
- `--node-pid <pid>`: check once a second whether that pid is gone (exited,
  a zombie, or reused by another process) and exit when it is. A pid whose
  details cannot be read counts as alive.

A bad value for any flag logs one line and exits 1.

## The pid file

Before it binds the socket, the binary writes a file named `pid` into the
directory that holds the socket: one line, `<pid> <start>\n`, its own pid
and its start time as an opaque integer (Linux: field 22 of
`/proc/<pid>/stat`; macOS: the BSD process start time in microseconds). It
writes `pid.tmp` first and renames it. The directory must already exist, be
writable, and be private to the host, like the socket. Any failure, including
an unreadable own start time, is fatal: one line naming the cause and the
path, then exit 1. Nothing removes the file on exit.

## Tests

`cargo test --manifest-path crates/Cargo.toml -p iso4-server` covers the
handshake, frame framing, payload parsers and encoders, and the demux
against a real engine over a socket pair. The end-to-end behavior is
covered by the TypeScript suites in `packages/iso4-sandbox/tests`, which
spawn this binary.
