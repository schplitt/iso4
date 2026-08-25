---
"@iso4/fetch": patch
---

fix: resolve DNS only for authorized requests

DNS is no longer resolved before the allow/deny check, so a denied host is
never looked up — closing a covert-lookup channel and an internal-network
oracle. The private/reserved-IP block now runs at connection time (for allowed
requests only) and `SafeFetchRequest.resolvedIp` is always `null`.
