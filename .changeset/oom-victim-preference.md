---
"@iso4/sandbox": patch
---

feat: the runtime child marks itself as the preferred OOM victim (#77)

On Linux the spawned runtime raises its own `oom_score_adj` at startup, so
a container out-of-memory kill takes the sandbox child (failed runs, a
respawnable sandbox) instead of the Node host.
