---
"@iso4/sandbox": patch
---

perf: receive large results without stalling the host

Incoming chunks are joined once per frame instead of re-concatenated per
chunk. A 15 MB result now takes ~19 ms rather than ~1.2 s.
