---
"@iso4/sandbox": patch
---

fix: a bridge handler settling after its run completed releases its streams (#127)

A streamed body returned by a handler whose run already finished no longer
leaves its host-side reader locked; the source is released since nothing
will ever pump it.
