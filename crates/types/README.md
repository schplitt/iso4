# iso4-types

The plain Rust types that the engine and every frontend agree on. One file,
`src/lib.rs`, no dependencies.

## What is in it

- **Resource limits** and their defaults: memory, CPU time, wall time,
  bridge call counts and sizes, stdout and stderr caps. The TypeScript
  client sends only what the caller set; absent fields are filled from the
  defaults here. `packages/iso4-sandbox/src/types.ts` documents the same
  numbers and points at this file as the source of truth.
- **The shape of a prefix**: host globals, host modules, import bindings,
  and the rebind rules a call may apply.
- **Run requests and completions**: what a one-off run or a prefix call
  asks for, and what comes back (exports, console lines, timings, bridge
  call records, or a failure with its error payload).
- **Bridge records**: which host target a call hit and how it ended.
- **Error payloads** and the reset causes carried on `ERR_INSTANCE_RESET`.

## What is not in it

No V8, no sockets, no encoding. How these types are laid out on the wire is
the server's business (`crates/server/src/ipc.rs` and `wire.rs`, documented
in `docs/protocol.md`). How they are enforced is the engine's
(`crates/core`).

## Who uses it

`iso4-core` re-exports it as `crate::types`. `iso4-server` parses frames
into these types and encodes them back out. A future frontend or pool
supervisor depends on this crate alone when it only needs to talk about
limits and prefixes without linking V8.

The `test-support` feature adds small constructors for tests in other
crates. It is never on in a build.
