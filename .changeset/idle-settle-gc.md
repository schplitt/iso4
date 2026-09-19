---
'@iso4/sandbox': patch
---

perf: collect an idle instance's garbage instead of holding it until eviction

A warm instance kept the last call's garbage for as long as it stayed resident, so the container never gave that memory back after a burst — V8's own memory reducer never runs here, because nothing pumps the platform loop. An instance that has been idle for 30 s now fires one low-memory notification, re-measures, and reports the settled heap before parking. Nothing the prefix still reaches is freed: module state, caches and compiled code survive, and the instance stays warm. The settled reading is reported through `stats().idleHeapBytes`, which now answers "what is still reclaimable" honestly; eviction deliberately keeps scoring on the last call's reading, so that it compares every candidate on the same basis. Disposal additionally trims the runtime's malloc arenas.

Measured in a 1 GB container, 400 heavy calls across 4 instances then idle: the runtime's resident memory held 195–220 MB indefinitely before and 33 MB after, and the registry's idle heap read 93–116 MB before and 1.4 MB after. The threshold is 30 s because a settle shrinks the heap — the first allocating call after one has to grow it back (a call allocating ~16 MB measured ~3 ms before a settle and ~12–15 ms directly after), so only a genuinely parked instance should pay it.
