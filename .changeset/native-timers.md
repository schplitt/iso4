---
"@iso4/sandbox": minor
---

feat: native `setTimeout`/`clearTimeout` in run code (#79)

Timers run natively in the runtime's event loop: a sleep costs no CPU budget but counts against the wall budget, `Date.now()` advances by exactly the requested delay, and a run's pending timers die with it. Both names are now reserved global names; `setInterval` is deliberately not provided.
