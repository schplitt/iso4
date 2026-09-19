---
'@iso4/sandbox': patch
---

feat: report how long a run waited for a slot

A run that had to queue for admission now carries `queueWaitMs` on its result; a run admitted straight away carries no such field. The wait sits outside `durationMs`, `wallTimeMs` and `cpuTimeMs`, which the runtime measures from dispatch onwards.
