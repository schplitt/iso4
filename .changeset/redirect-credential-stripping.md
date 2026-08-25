---
"@iso4/fetch": patch
---

fix: strip credentials on cross-origin redirects

Following a redirect to a different origin now drops `authorization`, `cookie`
and `proxy-authorization`, and a method-changing redirect drops `content-*`
headers, matching undici's redirect handling.
