---
"@iso4/sandbox": patch
---

fix: streamed request bodies arrived truncated when concurrent streaming runs shared a warm instance (#170)

A run joining a busy instance could silently lose its first body chunks to a delivery race inside the runtime's instance loop; jobs and frames now share one ordered channel, so a run is always registered before its frames arrive.
