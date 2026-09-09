---
"@iso4/sandbox": minor
---

feat: dedicated error codes for reserved-name shadowing and invalid rebinds (#162)

`ERR_UNDECLARED_BINDING` previously covered three unrelated failures. It now
means only what it says — using a binding that was never declared — and two
new codes take over the rest:

- `ERR_RESERVED_NAME`: a host global shadows a runtime-owned name
  (`console`, `setTimeout`, `Response`, …).
- `ERR_FROZEN_BINDING`: a `prefix.run()` rebind targets a declared location
  that is frozen with the prefix (a source module or a data leaf).
- `ERR_INVALID_REBIND`: a rebind offered a non-function value for a function
  leaf (host-detected, never sent by the runtime).

**Breaking for error-code matching:** consumers branching on
`ERR_UNDECLARED_BINDING` for the reserved-name or frozen-rebind cases must
match the new codes instead.
