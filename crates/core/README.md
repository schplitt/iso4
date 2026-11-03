# iso4-core

The engine. Everything that runs sandboxed JavaScript lives here; nothing
that talks to a socket does.

## What it does

A frontend hands `engine::Engine` a prefix to store, or a run to execute,
together with a `link::HostLink`: the one channel through which the engine
asks the host something (a bridge call, a stream chunk) and answers
(result, completion). The engine never sees a frame. The server crate
encodes those messages for Node; another frontend could encode them
differently or not at all.

Inside, the engine owns:

- **Isolates and limits** (`v8/runloop.rs`, `v8/instance.rs`, `v8/isolate.rs`,
  `v8/guard.rs`): one OS thread per isolate, heap limits, CPU budgets that
  stop counting while the run waits on the host, wall deadlines on a shared
  watchdog thread, the turn loop that drives a run.
- **The prefix store and warm instances** (`prefix.rs`, `warm.rs`,
  `engine.rs`): a prepared prefix is kept as source, evaluated per isolate,
  and served by resident warm instances that skip that evaluation on reuse.
- **Capacity policy** (`policy.rs`, `container.rs`, `rss.rs`): eviction and
  admission decisions against the container's memory, measured from the
  cgroup where one exists.
- **The web runtime** (`webtypes.rs`, `webtypes.js`, `url.rs`, `v8/console.rs`,
  `v8/timers.rs`, `v8/globals.rs`, `v8/streams.rs`): the globals every isolate
  gets, installed natively into the build-time snapshot (`snapshot.rs`, built
  by `build.rs`).
- **The value codec** (`blob.rs`, `webcodec.rs`): V8 serialization for
  everything that crosses the sandbox boundary, plus the descriptors for
  host types like `Request` and `Response`.
- **The frozen clock** (`clock_platform.cc`): a V8 platform whose wall clock
  is the isolate's own, so `Date` and friends stay the unmodified builtins.

## What it does not do

It does not open sockets, parse frames, or know the protocol version. It
does not decide who answers a bridge call; the frontend does. It holds no
network policy; `fetch` is a host-configured bridge global like any other.

## Layout

- `src/lib.rs` lists the modules and embeds the snapshot `build.rs` makes.
- `src/types.rs` re-exports `iso4-types`, so the modules keep saying
  `crate::types`.
- `src/errors.rs` holds `RunError`, the failure output and the error codes.
- `src/v8/` is the engine: `mod.rs` only lists the modules and re-exports;
  `runloop.rs` holds the run entry points and the turn loop; `instance.rs`
  the owner thread that serves a warm instance; `isolate.rs` creates
  isolates and warm instance cores;
  `runtable.rs` keys everything a native callback reaches by the owning run
  and carries that run's token through its continuations; `exceptions.rs`
  turns thrown V8 values into `RunError`s;
  `oom_label.rs` writes the fatal OOM line; `console.rs`, `timers.rs` and
  `guard.rs` hold the console capture, the guest timers and the limit
  guard; `globals.rs` installs host globals and
  `waitUntil`; `modules.rs` evaluates prefixes, resolves imports and
  extracts exports; `bridge.rs` carries calls in both directions;
  `streams.rs` moves streamed bodies; `node/` holds the `node:*` modules the
  sandbox mirrors, one file per specifier, starting with `async_hooks.rs`;
  `testsupport.rs` (test only) holds the run shorthands and the in-memory
  host end of a link that every engine test module uses.
- `benches/micro.rs` is the criterion suite CodSpeed runs in CI.
- `tests/wpt/` is the vendored web-platform-tests URL corpus the `URL` tests
  run against.

## Tests

`pnpm test:native` from the repo root, or
`cargo test --manifest-path crates/Cargo.toml -p iso4-core`. The tests drive
runs through an in-memory `HostLink` (`ChannelLink` in `link.rs`, wired up
by `test_io` in `v8/testsupport.rs`) and never touch a socket. The
`test-support` feature exposes `testval`, a test-only value tree for
asserting on serialized blobs from other crates' tests.

## Read next

`DESIGN.md` §3 (execution model), §4 (limits, globals, imports), §11
(the prefix pattern and the snapshot), §13.2.1 (warm instances).
