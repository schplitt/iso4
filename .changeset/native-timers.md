---
"@iso4/sandbox": minor
---

feat: native `setTimeout`/`clearTimeout` in the sandbox (#79)

`await new Promise(r => setTimeout(r, 500))` now works in run code —
implemented natively in the runtime's event loop (no bridge call, no host
round-trip). Sleeping costs no CPU budget but counts against the wall
budget, and timers registered by `waitUntil` work keep firing during the
grace phase under `graceMs`. Timer ids are per-run numbers; a run's pending
timers are dropped when it settles and can never fire into a later run.
Both names join the reserved-globals list, so a host global named
`setTimeout` or `clearTimeout` is now rejected. Timers are virtualized onto
the sandbox's frozen clock (`Date.now()` across a 500 ms sleep advances by
exactly 500), and at most 10,000 timers may be pending per run.
`setInterval` is deliberately not provided.
