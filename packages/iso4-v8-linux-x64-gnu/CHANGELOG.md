# @iso4/v8-linux-x64-gnu

## 0.2.2

### Patch Changes

- 9958d76: Restore thrown-error shape across the bridge: direct props in the sandbox, `error.fields` on the host.

  - **Host → sandbox:** a host handler error's own-enumerable properties (e.g. `status`, `reason`, a custom `reasoning`) are now re-attached as **direct own properties** on the Error the sandbox catches — `e.status` instead of the previous `e.data.status`. Reserved keys (`name`/`message`/`stack`/`__proto__`) can never be injected through the payload. Rethrowing the caught error (or spreading it into a fresh one) round-trips all fields to the host.
  - **Sandbox → host (breaking rename, pre-1.0):** `RunResult.error.data` is now `RunResult.error.fields`, typed `Record<string, unknown>` — a record of _all_ extra own-enumerable props, so a thrown error's own `data` property lands as `fields.data` and nothing can collide with `error.code`. `stack` stays a dedicated top-level `stack?: string`; `name`/`message`/`stack` are reserved and never appear inside `fields`.
  - **Thrown primitives fixed:** `throw "some string"` in the sandbox now produces a clean `{name: 'Error', message: 'some string'}` — previously it carried a literal `"undefined"` stack and the string's character indices as data.

## 0.2.1

### Patch Changes

- 00f072d: Propagate host-handler error `name`, `message`, and `data` across the bridge (#22).

  When a host global/import handler throws, the sandbox now receives a real `Error` whose `name` matches the thrown error (built-in names like `TypeError` use the matching constructor, so `instanceof` works), with own-enumerable properties beyond `name`/`message`/`stack` carried as `e.data`. The host **stack is deliberately never sent** into the sandbox — it can expose host file paths and infrastructure details.

  Behavior changes (breaking, pre-1.0):

  - **Host handler errors are now catchable in the sandbox.** Previously any handler error terminated the run even when sandbox code caught it. Uncaught handler errors still fail the run with `ERR_HOST_BRIDGE`, now with `name`/`message`/`data` preserved on `RunResult.error` instead of the flattened `name: 'Error'`. Bridge limit violations (`maxBridgeCalls`, `maxBridgeCallBytes`, function arguments) remain fatal.
  - The `BridgeResponse` wire format gained an error `data` field (protocol version stays at 1 pre-1.0). `@iso4/sandbox` and the `@iso4/v8-*` binaries must be upgraded together — they are version-fixed and release in lockstep.

## 0.2.0

## 0.1.0

### Minor Changes

- 91fc138: feat: propagate JS error name and structured data across the sandbox bridge

  `RunError.name` now reflects the actual JS error name (`TypeError`, `RangeError`, custom names via `error.name = '…'`) instead of always being `'Error'`. For `ERR_USER_CODE` errors, a new optional `data` field carries the thrown error's own enumerable properties (excluding `name`, `message`, `stack`) serialised via the same WireValue path as exports — functions and other non-serialisable values are silently dropped.

  Non-object throws (`throw 'string'`, `throw 42`) keep `name: 'Error'` and `data: undefined`.

## 0.0.4

### Patch Changes

- fbe4332: feat: import support

## 0.0.3

### Patch Changes

- 7c943af: chore: add bin field to native packages

## 0.0.2

### Patch Changes

- 58c27ad: ci: set execute bit on native binaries

## 0.0.1

### Patch Changes

- 88554a4: initial release
