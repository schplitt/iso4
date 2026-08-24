---
"@iso4/sandbox": minor
---

refactor: remove runtime V8 snapshot creation — prefixes are validated source, re-evaluated per run (#60, #61, #62)

The public API and the wire protocol are unchanged, but per-run latency now
includes prefix evaluation, and a nondeterministic prefix produces per-run
values. Closes the intermittent child-process crash under concurrent
`prepare()`.
