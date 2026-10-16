---
"@iso4/sandbox": patch
---

fix(sandbox): refuse arrays with holes in bridge call arguments (#219)

A bridge global or import called with a holey array anywhere in its arguments now rejects with a catchable `TypeError` before the host handler runs.
