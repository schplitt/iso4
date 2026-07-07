---
"@iso4/sandbox": minor
---

chore: add `status` discriminant and abort `reason` to `RunResult`

`RunResult` now carries an explicit `status: 'completed' | 'failed' | 'aborted'`, promoting a deliberate abort to a first-class outcome instead of leaving it indistinguishable from a genuine failure. Aborted runs additionally expose `reason` — the value passed to `AbortController.abort(reason)`.

This is **additive and backward compatible**:

- `ok` stays as a convenience alias for `status === 'completed'`, so `if (result.ok)` is unchanged.
- Aborted results keep `error` with `code: 'ERR_ABORTED'`, so existing `!result.ok && result.error.code === 'ERR_ABORTED'` checks keep working.
- New code can switch on `result.status` and, for aborts, read `result.reason`.

```ts
const result = await sandbox.run({ code, signal })
switch (result.status) {
  case 'completed': use(result.exports); break
  case 'failed':    handle(result.error); break
  case 'aborted':   suspend(result.reason); break  // reason = whatever abort(reason) received
}
```
