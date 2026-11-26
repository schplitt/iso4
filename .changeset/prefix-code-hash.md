---
"@iso4/sandbox": minor
---

feat(sandbox): calling a prefix the runtime no longer holds returns a failed result with `ERR_PREFIX_UNKNOWN`

Before, it threw `ERR_PREFIX_DISPOSED`. Calling a handle you disposed yourself still throws `ERR_PREFIX_DISPOSED`.
