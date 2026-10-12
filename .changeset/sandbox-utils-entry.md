---
'@iso4/sandbox': patch
---

feat(sandbox): add `@iso4/sandbox/utils` with `isValidExportName` and `isValidGlobalName`; the name constants now live there, and `NaN`, `undefined`, `Infinity` and `__proto__` are refused as global names.
