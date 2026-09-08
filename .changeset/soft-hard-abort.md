---
"@iso4/sandbox": minor
---

feat: soft and hard aborts — `signal` becomes a boundary abandon, new `hardAbortSignal` escalation (#75)

The default `signal` abort is now soft: it lands at the run's next execution
boundary as a clean abandon, so nothing is ever interrupted mid-execution,
the warm instance survives, and co-resident runs are untouched — a
synchronous loop with no `cpuTimeMs` cap is reachable only by the new
`hardAbortSignal`, which interrupts immediately and resets the shared
instance when it lands mid-execution (`ERR_INSTANCE_RESET`, cause `abort`).
This also fixes aborts that landed inside a resumed continuation being
silently swallowed — the run hung and the 100 ms fallback tore down the
whole connection; that fallback now arms for hard aborts only.
