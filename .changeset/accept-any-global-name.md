---
"@iso4/sandbox": patch
---

fix(sandbox): accept any string as a host global name again and drop `ERR_INVALID_GLOBAL_NAME`.
A name that is not an identifier, such as `my-tool`, is reachable as `globalThis['my-tool']`; a global name or import specifier that is not a well-formed string throws a `TypeError`.
