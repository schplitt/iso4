---
"@iso4/sandbox": minor
---

feat(sandbox): remember a prefix whose cold start failed and refuse later runs with `ERR_PREFIX_BROKEN`

The failing run's error now carries `stage: 'prefix'`, `Prefix` gains `status` and `error`, and later runs reject with `ERR_PREFIX_BROKEN` (`cause` = the stored failure) instead of booting an isolate that fails again. Only the prefix's own repeatable errors mark it; `ERR_WARMUP_LIMIT` and `ERR_MEMORY_LIMIT` do not.
