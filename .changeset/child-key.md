---
"@iso4/sandbox": patch
---

feat(native): the runtime takes an optional `--key` and checks it on every `Run` and `Precompile`

Both frames now carry an optional key after the run id or prefix id; Node sends none, so nothing changes for callers. A runtime started with `--key` refuses a frame whose key differs with `ERR_INTERNAL`, and prints the key on its crash line.
