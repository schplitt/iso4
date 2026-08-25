---
"@iso4/sandbox": patch
---

fix: reject URL credentials and control-char statusText in the sandbox

`new Request(url)` now throws when the URL includes credentials (fetch spec),
and `new Response` rejects a `statusText` containing control characters
(mirroring workerd) — so these fail on the user's line rather than host-side.
