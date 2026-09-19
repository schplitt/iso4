---
'@iso4/sandbox': patch
---

fix: weight age above size when choosing which idle instance to evict

The victim score was `heapUsed × idleTime`, which treats a megabyte and a second as interchangeable. They are not: a large instance is often just an infrequently called one that is genuinely in use, while a long-idle instance is simply unused. The score is now `heapUsed × idleTime^1.2`, so age wins once the ages differ enough — a 100 MB instance idle 20 s is no longer evicted ahead of a 30 MB instance idle 60 s. The exponent is what carries the change; a plain multiplier would scale every score alike and reorder nothing.
