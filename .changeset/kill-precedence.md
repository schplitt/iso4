---
"@iso4/sandbox": patch
---

fix(sandbox): settle sync-call microtasks within the call, fix kill precedence and a multibyte grace crash

Microtasks a synchronous `prefix.call` queues now settle inside that call instead of the next one, a CPU/wall/memory kill wins over a bridge error raised in the same turn, and a multibyte `waitUntil` error no longer crashes the runtime. An uncloneable bridge argument is now `ERR_EXPORT_NOT_SERIALIZABLE` (was `ERR_INTERNAL`).
