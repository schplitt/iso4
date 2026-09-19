---
'@iso4/sandbox': patch
---

perf: an idle instance can collect its garbage instead of holding it until eviction

A warm instance keeps the last call's garbage for as long as it stays resident: V8 collects on allocation, and an idle instance never allocates, so nothing triggers it. The runtime can now fire one low-memory notification on an instance that has been parked for a given number of seconds, re-measure, and report the settled heap — module state, caches and compiled code all survive, and the instance stays warm. Disposal additionally trims the runtime's malloc arenas.

**Off by default**, behind the runtime's `--idle-settle-secs` flag, with no host-side option yet. A dropped instance's pages go back to the operating system on their own within about twenty seconds, so the settle only pays for itself where warmth has to stay resident in a container tight enough to feel the garbage. Measured there it is worth a lot — in a 1 GB container running 400 heavy calls across 4 instances, resident memory held 195–220 MB before and 33 MB after, and the registry's idle heap read 93–116 MB before and 1.4 MB after. The cost is that a settle shrinks the heap, so the first allocating call afterwards has to grow it back: a call allocating ~16 MB measured ~3 ms before a settle and ~12–15 ms directly after.

Also changes the eviction score from `heapUsed × idleTime` to `heapUsed × idleTime^1.2`, so age outweighs size — a fat instance may be infrequently but genuinely used, while an old one is simply unused.
