---
"@iso4/sandbox": patch
---

fix: install runtime-internal globals non-enumerable

The `__iso4_*` plumbing no longer appears in `Object.keys(globalThis)` or `for...in`; host-declared globals stay enumerable like browser globals. Sandbox code that enumerates its environment no longer trips over runtime internals.
