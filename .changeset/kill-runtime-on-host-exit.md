---
"@iso4/sandbox": patch
---

fix: stop the runtime when the host exits without `dispose()`

A last-resort `exit` hook now ends it. `dispose()` is still the only complete
answer, since a host killed by a signal runs no JavaScript.
