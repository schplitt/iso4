---
"@iso4/fetch": patch
---

fix(fetch): never quote the URL or a header value in an error message.
A URL with credentials is now refused with `fetch: URL must not contain credentials` by both `createSafeFetcher` and `createSafeFetch`; the handler used to drop them silently.
