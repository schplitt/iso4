---
"@iso4/sandbox": patch
---

fix: top-level `await` in prefix code no longer fails `prepare()` (#55)

Any top-level `await` — even `await 1` — used to fail `prepare()`/`precompile()`
with `ERR_EXPORT_NOT_SERIALIZABLE: "module evaluation promise is still pending"`,
because nothing drained the microtask queue after the prefix module was
evaluated. This blocked preparing bundled handlers, which commonly carry
top-level await.

The runtime now drains the microtask queue (bounded) until the prefix's
evaluation promise settles, so `await 1`, `await Promise.resolve(x)`,
`await response.text()`, chained awaits, and top-level await inside imported
source modules all work in prefix code.

Two new error codes make the remaining limits explicit:

- `ERR_PREFIX_BRIDGE_CALL` — prefix code *called* a bridge callable (bridge
  global, shim global, or host-import function) at `prepare()` time. No host
  session exists while the snapshot is built, so the call can never be served.
  Declared bridge callables are now installed as throwing placeholders during
  precompile: they exist (`typeof fetch` matches run() code) and can be
  referenced or stashed, but calling one fails with this code instead of an
  accidental `ReferenceError`/`TypeError`.
- `ERR_PREFIX_DID_NOT_SETTLE` — the prefix's evaluation promise can never
  settle (e.g. `await new Promise(() => {})`). The checkpoint loop is bounded,
  so `prepare()` cannot hang.
