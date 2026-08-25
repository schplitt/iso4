---
"@iso4/sandbox": patch
---

fix: reject a supplied-but-malformed per-run global override

A per-run `globals` override that is present but not a function (e.g. a tenant
handler that resolves to `undefined`) now throws instead of silently falling
back to the precompile-time default, matching the imports side.
