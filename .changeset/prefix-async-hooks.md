---
"@iso4/sandbox": minor
---

feat(sandbox): allow `node:async_hooks` (`AsyncLocalStorage`) in prefix code

Create the instance at module scope and use it inside an exported handler, as in workerd; importing it from a prefix no longer fails with `ERR_MODULE_NOT_FOUND`.
