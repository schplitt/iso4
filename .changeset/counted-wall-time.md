---
"@iso4/sandbox": minor
---

feat: three run-time numbers — `durationMs`, new `wallTimeMs`, `cpuTimeMs` — and a wall limit that counts only the run's own time (#148)

Every result (run, call, failure, abort, waitUntil report) now carries `wallTimeMs`: the run's own logic time — execution plus async waits, each wait counted until its answer arrived at the runtime. Engine time (turns for co-resident runs on a shared instance, delivery backlog) is excluded and visible as `durationMs - wallTimeMs`; `durationMs` keeps its meaning as the complete dispatch-to-conclusion elapsed time. `cpuTimeMs ≤ wallTimeMs ≤ durationMs`.

`limits.wallTimeMs` now caps this counted time instead of real elapsed time, so a run on a busy instance is never timed out for delays the engine caused; `graceMs` follows the same rule for the waitUntil phase. Timers and deadlines due before a newly submitted run's arrival now also fire before its start turn. Wire protocol version bumps to 2.
