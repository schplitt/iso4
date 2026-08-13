---
"@iso4/sandbox": minor
---

feat: capacity manager — memory budget and `sandbox.stats()` (#65)

Capacity is now two independent resources with two knobs, both automatic.
`maxIsolates` keeps its meaning — the number of *concurrent runs*
(connection slots). New **`memoryBudgetMb`** decides how many isolates may
stay *alive* — running plus the warm instances kept resident between prefix
runs — at `budget ÷ memoryMb`, never below `maxIsolates`. Idle warm
instances can therefore outnumber run slots: memory, not slot count,
decides how much stays warm. On an 8-slot sandbox this means a fleet of
prepared prefixes stays warm side by side instead of evicting each other on
every rotation.

The default budget is container-aware: `process.constrainedMemory()`
(cgroup/Docker-aware; `os.totalmem()` lies inside containers) minus a
safety net of max(512 MB, 25 %) for the Node host, the Rust runtime, and
the embedding service's own per-isolate state. Services with a large
per-isolate host cache should set the knob lower. Requires a nonzero
`memoryMb`; with uncapped isolates the live cap stays at `maxIsolates`.

Saturation behavior is unchanged and has no knobs: callers beyond
`maxIsolates` queue FIFO, a wait bounded in practice by the running calls'
own wall/CPU limits.

**`sandbox.stats()`.** A point-in-time capacity snapshot: `activeRuns`,
`queueDepth`, `warmInstances` / `idleInstances`, `idleHeapBytes` (measured,
not assumed), `maxLiveIsolates`, and per-prefix `{ idle, busy }` counts so
a saturated tenant is diagnosable. Served over a dedicated control
connection outside the run pool, so it answers even while every slot is
busy. Wire: new `Stats` (0x08) / `StatsResult` (0x06) frames; protocol
stays v2 (pre-release lockstep releases).

`maxIsolates` now defaults to `os.availableParallelism()` (was
`os.cpus().length`) — cgroup-CPU-aware, same number on bare metal.
