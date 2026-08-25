---
"@iso4/sandbox": patch
---

fix: preserve an own `__proto__` key crossing into the sandbox

Rebuilding a host-supplied plain object no longer triggers the prototype setter,
so an own-enumerable `__proto__` key (e.g. from `JSON.parse`) crosses as data
instead of being silently dropped.
