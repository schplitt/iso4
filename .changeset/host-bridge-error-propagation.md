---
"@iso4/sandbox": patch
"@iso4/v8-darwin-arm64": patch
"@iso4/v8-darwin-x64": patch
"@iso4/v8-linux-x64-gnu": patch
"@iso4/v8-linux-arm64-gnu": patch
---

Propagate host-handler error `name`, `message`, and `data` across the bridge (#22).

When a host global/import handler throws, the sandbox now receives a real `Error` whose `name` matches the thrown error (built-in names like `TypeError` use the matching constructor, so `instanceof` works), with own-enumerable properties beyond `name`/`message`/`stack` carried as `e.data`. The host **stack is deliberately never sent** into the sandbox — it can expose host file paths and infrastructure details.

Behavior changes (breaking, pre-1.0):

- **Host handler errors are now catchable in the sandbox.** Previously any handler error terminated the run even when sandbox code caught it. Uncaught handler errors still fail the run with `ERR_HOST_BRIDGE`, now with `name`/`message`/`data` preserved on `RunResult.error` instead of the flattened `name: 'Error'`. Bridge limit violations (`maxBridgeCalls`, `maxBridgeCallBytes`, function arguments) remain fatal.
- The `BridgeResponse` wire format gained an error `data` field (protocol version stays at 1 pre-1.0). `@iso4/sandbox` and the `@iso4/v8-*` binaries must be upgraded together — they are version-fixed and release in lockstep.
