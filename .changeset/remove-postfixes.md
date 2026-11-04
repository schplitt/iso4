---
"@iso4/sandbox": minor
---

feat(sandbox)!: remove `prefix.execute()` and the deprecated `prefix.run()`

A prefix is full code: runs call one of its exports with `prefix.call({ export, args })`. Move per-run code into an exported function on the prefix, or run it once with `sandbox.run({ code })`.
