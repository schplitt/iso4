---
"@iso4/sandbox": minor
---

feat!: replace the WireValue codec with V8 serialization blobs

Protocol version 1 → 2, so `@iso4/sandbox` and `@iso4/v8-*` must be updated
together and a mismatch now fails at `createSandbox()`. `Date`, `Map`, `Set`,
`RegExp`, `Error`, typed arrays and cycles round-trip as real instances, and
dense payloads are ~5.9× faster.
