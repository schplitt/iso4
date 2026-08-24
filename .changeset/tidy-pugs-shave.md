---
"@iso4/sandbox": minor
---

feat: sandbox web runtime — `Headers`, `Request`, `Response`, `URL`, `TextEncoder` and friends

`Request`, `Response` and `Headers` cross the boundary as real instances rather
than flattening to plain objects. New error code `ERR_TYPE_NOT_SERIALIZABLE`
for values that cannot cross; streams are deliberately unsupported.
