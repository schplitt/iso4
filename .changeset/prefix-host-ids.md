---
"@iso4/sandbox": minor
---

feat(sandbox): `prepare({ id, code })` takes a host-chosen prefix id, and `id` is now required

Calls to `prepare({ code })` without an `id` now throw a `TypeError`. Preparing an id still held with the same code and memory limits returns another handle to the same prefix (the last `dispose()` drops it); other code or other memory limits throw `ERR_PREFIX_ID_TAKEN`.
The runtime answers `DisposePrefix` with a new `DisposePrefixResult` frame, and the last handle's `dispose()` resolves once the runtime confirmed it, or once the connection or the runtime ended first.
