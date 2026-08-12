---
"@iso4/sandbox": minor
---

feat: warm isolate registry — prefix runs reuse resident isolates (#64)

Every prepared prefix is now served by **warm instances**: the first
`prefix.execute()` / `prefix.call()` cold-starts an isolate (prefix
evaluated once), later runs reuse it and skip isolate boot, the runtime
installs, prefix re-evaluation, and teardown. Automatic — no flag, no new
API; the runtime decides warm vs cold per run, invisible on the wire.

The contract is workerd's: **warmth is a cache, never a guarantee.**
Module-scope state MAY survive between runs on one instance — permitted,
unguaranteed, evictable at any time. Code that relied on a fresh isolate
per prefix run must not: put per-request state in the handler. The
supported pattern for expensive setup is lazy init inside the handler
(`conn ??= await setup()`), which re-runs correctly after any eviction.
Instances of one prefix share no state with each other; one call at a time
per instance — concurrency for one prefix means more instances. One-off
`sandbox.run()` is untouched: always a fresh isolate.

Reliability semantics:

- **Per-call limits unchanged**: fresh CPU/wall guards from dispatch to
  settle; bridge stubs and console caps re-armed per call.
- **Taint-and-evict**: any fired guard, an abort landing mid-call, or a
  fatal bridge violation discards the instance — the next run cold-starts
  clean. Ordinary uncaught exceptions do not taint.
- **Fixed warm-up budget** (1 s wall / 1 s CPU, not configurable —
  Cloudflare's script-startup model): prefix evaluation (and the
  per-instance runtime installs) runs under it, never billed to the
  triggering request. Isolate boot itself precedes the budget. Blowing the
  time budget reports the new **`ERR_WARMUP_LIMIT`**; blowing the heap cap
  during warm-up keeps `ERR_MEMORY_LIMIT`. `prepare()` enforces the same
  budget and heap cap, so an un-warmable prefix fails at deploy time — this
  also fixes a hang (a prefix with a synchronous infinite loop used to hang
  `prepare()` forever) and prevents a memory-heavy prefix from OOMing the
  runtime at `prepare()` time.
- **Prefix `console.log` output** now surfaces on the **first** run's result
  (the cold-start run that paid for warm-up), then is cleared. Previously
  prefix output appeared on every run (per-run re-evaluation); it must not
  now, since the prefix evaluates once per instance.
- **Capacity (v1)**: one cap — `maxIsolates` — covering running isolates
  plus idle warm instances; taking a slot at the cap evicts the
  least-recently-used idle instance (scored eviction lands with #66, the
  capacity manager with #65).

Breaking — **`limits.memoryMb` moved to `createSandbox({ memoryMb })`**
(default raised 64 → 128 MB, workerd's number): the heap cap is baked into
each isolate at creation and isolates are now reused, so a per-run value is
structurally impossible. The cap is uniform across all isolates of a
Sandbox; memory accumulates across runs on an instance, and hitting the cap
taints it. Passing the removed per-run field throws a `TypeError`.

New result field: **`heapUsedBytes`** — `used_heap_size` of the isolate
that served a prefix run, measured after it settled (absent for one-off
runs). Wire: the Result payload gains an `Optional<u64>`; protocol stays v2
(pre-release lockstep releases).
