---
"@iso4/sandbox": minor
---

feat: widen `HostExportData` to everything V8 serialization carries

`HostExportData` described only primitives, `bigint`, `Uint8Array`, and plain
objects/arrays — the limits of the codec that PR #48 deleted. It now describes
what actually crosses: `Date`, `RegExp`, `Error`, `Map`, `Set`, `ArrayBuffer`,
every `TypedArray`, `DataView`, and cycles, all arriving as real instances with
object identity preserved.

**Host-module data leaves are no longer inspected at registration.** The old
`processImports` walk recursed through every data leaf to reject `Date`, `Map`,
`Set`, cycles, and class instances — duplicating what the serializer does
anyway, on the Node main thread, at O(values). It is gone; the value goes
straight to V8's serializer, which is the single gate on what may cross.

Consequences, all additive except the last:

- `Date`/`Map`/`Set`/`RegExp`/`Error`/`ArrayBuffer`/`TypedArray` data leaves
  that previously threw at `run()`/`prepare()` now work.
- Cycles inside a data leaf now work. A cycle through a **plain object** still
  throws, because the shape walker must descend plain objects to find function
  leaves and cannot tell nested shape from cyclic data.
- Class instances no longer throw at registration. They flatten to their own
  enumerable properties, which is the documented boundary behavior
  (`docs/protocol.md` §4.2) — the registration-time rejection contradicted it.
- An unsupported value (a function nested in data, `Symbol`, `Promise`,
  `WeakMap`, `Proxy`) still fails before reaching the sandbox, but with the
  serializer's own data-clone error instead of one naming the exact leaf path.
