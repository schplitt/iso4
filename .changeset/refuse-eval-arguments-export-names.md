---
"@iso4/sandbox": patch
---

fix(sandbox): refuse `eval` and `arguments` as host-module export names up front instead of failing the run with `ERR_COMPILE`.
