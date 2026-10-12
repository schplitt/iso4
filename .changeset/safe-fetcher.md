---
'@iso4/fetch': minor
---

feat: add `createSafeFetcher`, a fetch-compatible function that runs the safe-fetch pipeline in host code

It takes the same options as `createSafeFetch` and resolves to a real `Response`.
