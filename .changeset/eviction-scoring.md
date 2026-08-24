---
"@iso4/sandbox": minor
---

feat: RSS watermark + scored eviction — `heapUsed × idleTime` (#66)

The memory budget is enforced against the runtime's own process RSS: at the
mark, idle warm instances are evicted by score and new warm admissions stop
until RSS falls back to 80 % of it. `sandbox.stats()` gains `budgetBytes`,
`rssBytes` and `underPressure`; the instance-count cap is gone.
