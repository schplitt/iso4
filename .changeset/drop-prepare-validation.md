---
"@iso4/sandbox": minor
---

feat(sandbox): prepare() no longer evaluates the prefix, and `ERR_COMPILE` is gone

`prepare()` stores the prefix without booting an isolate; a broken prefix now fails its first `execute()`/`call()` as an `{ ok: false }` result with the same codes it used to reject with. A syntax error anywhere reports as `ERR_USER_CODE` with `name: 'SyntaxError'` instead of `ERR_COMPILE`, with the file and line in `stack`. Runs that joined an instance whose cold start failed now get that failure instead of hanging.
