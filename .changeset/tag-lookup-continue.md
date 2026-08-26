---
"@iso4/sandbox": patch
---

fix: identify runtime web types independently of one another during serialization

Serializing a `Headers`, `Request` or `Response` no longer fails just because sandbox code removed or shadowed one of the other classes on `globalThis`.
