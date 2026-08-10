---
"@iso4/sandbox": minor
---

feat!: replace the WireValue codec with V8 serialization blobs

Values now cross the boundary as V8 serialization blobs instead of the
hand-written codec. Dense payloads round-trip **5.9× faster** (0.5 MB: 98 → 16 ms).

Protocol version 1 → 2 — **`@iso4/sandbox` and `@iso4/v8-*` must be updated
together**; a mismatch now fails loudly at `createSandbox()` instead of hanging.

No public API change. Behavior changes: `Date`, `Map`, `Set`, `RegExp`, `Error`,
`ArrayBuffer`, all `TypedArray`s, and cycles now round-trip as real instances
(previously rejected); class instances flatten to their own enumerable
properties; `__proto__` as an own key passes through as plain data. Functions,
symbols, and promises are still rejected loudly.
