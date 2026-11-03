---
"@iso4/sandbox": patch
---

fix(sandbox): name the missing module in `ERR_MODULE_NOT_FOUND` and report other import failures as the new `ERR_IMPORT`

Dynamic `import()` now ends the run with `ERR_IMPORT` instead of throwing a catchable error, and co-resident runs on the same instance get `ERR_INSTANCE_RESET` with the new cause `'unsupported'`.
