---
"@iso4/sandbox": patch
---

feat(native): the runtime takes a required `--id` and an optional `--kind`, and reports its id, kind and key on every `Hello`

`Authenticate` now carries a role byte after the protocol version (Node sends `node`; a `supervisor` skips the V8 probe comparison), and Node starts each runtime with its own `--id`. Nothing changes for callers.
