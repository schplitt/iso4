---
"@iso4/sandbox": patch
---

fix: the sandbox console keeps V8's full method set (#86)

`console.table`, `console.group`, `console.count`, `console.time` and the rest are now present and callable instead of `undefined` — the runtime wraps five logging methods on V8's own console object rather than replacing it, so an unwrapped method is an inert no-op rather than a `TypeError` in library code that calls it. `log`/`debug`/`info`/`warn`/`error` capture to `stdout`/`stderr` exactly as before.

The `prepare()` validation isolate installs the same console, so a prefix can no longer validate against a method that is missing at run time. `docs/conformance.md` is new and carries the per-method table alongside the rest of the guest-visible surface.
