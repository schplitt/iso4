---
"@iso4/fetch": patch
---

fix: reach origins that resolve to IPv6

DNS pinning forced IPv4, so an IPv6-only host was unreachable and could end
the host process. Literal IPv6 URLs now resolve too, and `::` is treated as
reserved.
