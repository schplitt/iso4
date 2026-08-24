---
'@iso4/sandbox': patch
---

perf: cut fixed per-run overhead — ~4 % hot-run latency, ~6 % throughput

Per-run trace logs are off by default; set `ISO4_V8_TRACE=1` to restore them.
Prefix snapshots are shared by handle instead of copied twice per run.
