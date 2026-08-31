---
"@iso4/sandbox": patch
---

fix: identify runtime web types by an internal construction-time tag

Serializing a `Headers`, `Request` or `Response` no longer depends on the classes being intact on `globalThis`, and overriding `Symbol.hasInstance` can no longer change how an instance crosses the boundary.
