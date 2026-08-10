---
'@iso4/sandbox': patch
---

Cut two sources of fixed per-run overhead in the V8 runtime: ~4 % off hot-run
latency and ~6 % off aggregate throughput.

**Per-run trace logs are now off by default.** The runtime wrote two unbuffered
`eprintln!` lines per run (`Run`/`PrefixRun` received, then succeeded/failed) to
a stderr that the host process inherits. Measured cost: ~13 µs/run when stderr
is discarded, ~23 µs when it is a real file or pipe — 2–4 % of a hot
`prefix.execute()`. Set `ISO4_V8_TRACE=1` to get them back.

Everything else the runtime logs is unchanged and still unconditional:
handshake failures, protocol violations, prefix lifecycle, and frame-write
errors. Those are rare, and they are the only signal for failures the host
cannot observe from a `Result` frame.

**Prefix snapshots are shared by handle instead of copied twice per run.** Each
`prefix.execute()` copied the ~460 KB startup snapshot once out of the prefix
store (while holding the store's lock, which every pool slot contends on) and
once more to hand to V8's `CreateParams`. The snapshot is now an `Arc<[u8]>`
that travels from the store into `CreateParams` without either copy — rusty_v8's
`snapshot_blob` accepts an `Arc` allocation directly, so V8 gets the same
pointer. At ~9,000 events/sec this removes ~8 GB/sec of memcpy and shortens the
store lock to a refcount bump.

No API or behaviour change beyond the default log verbosity.
