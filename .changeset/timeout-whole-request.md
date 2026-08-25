---
"@iso4/fetch": patch
---

fix: `timeoutMs` bounds the whole request, not each redirect hop

A single deadline now spans the entire redirect chain, so a redirecting request
can no longer run for `(maxRedirects + 1)` times the configured timeout.
