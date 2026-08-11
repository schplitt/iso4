---
"@iso4/sandbox": minor
---

chore: bump the v8 crate 130 → 147 (V8 13.0 → 14.7)

The sandbox now runs **V8 14.7** instead of 13.0 — 17 major versions of JIT, GC,
and security work, and the JS surface that came with them. The serialization
format version is unchanged (15), so no protocol change and no handshake impact:
`@iso4/sandbox` and `@iso4/v8-*` still pair exactly as before, and every Node
from 20 through 26 writes the same format.

147 is the newest crate that still writes format 15. V8 **14.9** (crate 149)
bumps it to 16, which a Node 26 host cannot read — so 149+ is gated behind
either a Node bump or format negotiation.

Measured, native micros:

| | 130 | 147 | |
|---|---|---|---|
| `snapshot_restore` | 984 µs | 694 µs | −36 % |
| `context_create` | 268 µs | 197 µs | −29 % |
| `call_per_event` | 1.11 µs | 906 ns | −17 % |
| `function_call` | 63.5 ns | 58.1 ns | −10 % |
| deserialize, byte-heavy | 96.2 µs | 54.9 µs | −47 % |
| deserialize, value-dense | 16.8 µs | 9.50 µs | −44 % |
| serialize, value-dense | 2.65 ms | 1.99 ms | −25 % |
| serialize, string-heavy | 761 ns | 2.12 µs | **+163 %** |

End-to-end nothing regressed: hot run +13 %, bridge round trip +11 %,
string-heavy round trip +45 %, cold start flat. The string-heavy serialize
regression is real but is a small enough slice of a round trip that it does not
surface — the same shape end-to-end is 45 % faster.

One deliberate regression: **the prefix snapshot is copied once per isolate
again.** `CreateParams::snapshot_blob` now takes a `StartupData`, which can only
wrap a `Cow<'static, [u8]>` — the `Arc` variant of the old `Allocated<[u8]>`
trait is gone, so the second of the two copies removed in #51 is back (~530 KB
per prefix run). `snapshot_restore` still improved 36 % net. Removing it again
means handing V8 a `Cow::Borrowed` over the cached `Arc`, which is only sound if
a prefix cannot be disposed while a run holds it — an invariant the runtime does
not establish today.

Also in this bump: V8 dropped `ArrayBuffer::Allocator::Reallocate`, so the
memory-budget allocator no longer has a `reallocate` hook. Resizes run through
`allocate` + `free`, so the budget still sees every byte, and sees the new size
before the old one is released.

**The host-type shells now zero their internal field.** `Headers`, `Request` and
`Response` extend a native shell that declares one internal field purely so V8
routes the object to `WriteHostObject`; the field carried no data and was never
written. That is no longer safe. `rusty_v8` documents
`get_aligned_pointer_from_internal_field` as undefined behaviour unless
`SetAlignedPointerInInternalField` wrote the field first, and since V8 14.7 the
read decodes through the *tagged* external-pointer table instead of loading a
raw word. V8's snapshot callback does exactly that read, so an unwritten field
handed V8 a garbage pointer and `CreateBlob` died with SIGBUS in
`ReadOnlyPromotion::Promote` whenever a prefix snapshot contained a live
instance. On V8 13.0 the same read happened to yield null, which is why the
never-write design worked there. The shells now write a null aligned pointer at
construction — the same write the crate performs when restoring an empty
payload. The field still carries no data; the type tag still comes from an
`instanceof` check.
