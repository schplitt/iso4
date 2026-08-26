---
"@iso4/sandbox": patch
---

fix: replace the argv auth token with a kernel-enforced private socket directory

The runtime socket now lives in a fresh owner-only (0700) per-sandbox directory, so access is enforced by the kernel at connect time. The token is gone from the spawn args and the wire handshake (`@iso4/sandbox` and `@iso4/v8-*` are released in lockstep).
