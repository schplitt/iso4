---
"@iso4/sandbox": patch
---

fix: rehydrate only host-emitted host-type descriptors, stamped per session

Host-to-sandbox `Headers`/`Request`/`Response` descriptors now carry a random per-sandbox stamp negotiated at connection setup, and the runtime rebuilds only stamped descriptors. Structured data passed into a run can no longer be reinterpreted as a host type, and no property name is reserved anymore.
