---
"@iso4/sandbox": patch
"@iso4/v8-darwin-arm64": patch
"@iso4/v8-darwin-x64": patch
"@iso4/v8-linux-arm64-gnu": patch
"@iso4/v8-linux-x64-gnu": patch
---

perf: one shared watchdog thread now enforces all CPU/wall budgets (#145)

Warm instances hold one OS thread instead of two, so deployments with hard
per-container thread limits can keep roughly twice as many instances warm.
Budget and timeout semantics are unchanged.
