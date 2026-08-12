---
"@iso4/sandbox": minor
---

feat: host → sandbox function calls — `prefix.call({ export, args })` and `run({ code, call })` (#58)

There was no way to call a function that already lives inside the sandbox:
the bridge runs sandbox → host, and `run()` is whole-module evaluation with
no argument channel. This adds the other direction, making
`export default { fetch(request) }` a first-class shape.

New API:

- **`prefix.call({ export, args, globals?, imports?, limits?, signal? })`** —
  evaluate the prepared prefix and invoke the export at the dot-separated
  path (`'default.fetch'`, `'handler'`), addressed **relative to the module's
  exports**, never `globalThis`. Nothing is compiled per request.
- **`sandbox.run({ code, call })`** — same, resolved against the freshly
  evaluated module.
- Both resolve to a `CallResult`: on success the called function's return
  `value` (awaited first when it is a Promise) replaces `exports` — never
  both. Logs, timings, and `bridgeCalls` are unchanged.
- Arguments cross as **one** V8 blob holding the array (identity preserved),
  through the host-type-aware leg: a real `Request` crosses in and a real
  `Response` crosses back. The receiver is the object the final path segment
  was read from, so handlers reading `this` work, and prototype methods
  resolve (`export default new Worker()`). Return values respect
  `maxExportBytes`.
- New error code **`ERR_CALL_TARGET_NOT_FOUND`**: the path does not resolve,
  or resolves to something not callable (including `export default class`,
  whose methods live on instance prototypes).
- **`sandbox.readExports({ code })`** — deploy-path declaration reader: load
  a module once, get `{ exports, skippedExports }` back; rejects on broken
  modules like `prepare()`.

Behavior change — **non-serializable exports are skipped, not fatal**: a
plain run of a module whose export cannot cross (a function, an unresolved
Promise, `export default { fetch }`) no longer fails with
`ERR_EXPORT_NOT_SERIALIZABLE`. The offending export is absent from `exports`
and its name is reported in the new `RunSuccess.skippedExports`, so nothing
is silently hidden. A call's return value stays loud — there the value *is*
the result.

Wire protocol changes (still v2 — in development): `Run`/`PrefixRun` gain an
optional `call` slot, `PrefixRun.code` becomes optional, and the success
payload gains `skippedExports`. `@iso4/sandbox` and `@iso4/v8-*` are released
in lockstep; the handshake rejects mismatched halves loudly.
