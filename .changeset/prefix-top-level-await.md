---
"@iso4/sandbox": patch
---

fix: top-level `await` in prefix code no longer fails `prepare()` (#55)

The runtime now drains the microtask queue until the prefix's evaluation
promise settles. Two new error codes state the remaining limits:
`ERR_PREFIX_BRIDGE_CALL` and `ERR_PREFIX_DID_NOT_SETTLE`.
