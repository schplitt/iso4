---
"@iso4/fetch": patch
---

fix: a matched host claims the origin for scheme and port

A request whose host matches a rule but uses a disallowed scheme or port is now
denied, matching the route behaviour, instead of falling through to the
`policy` callback.
