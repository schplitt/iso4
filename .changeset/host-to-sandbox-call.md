---
"@iso4/sandbox": minor
---

feat: host → sandbox function calls — `prefix.call({ export, args })` and `run({ code, call })` (#58)

Call a function that already lives in the sandbox, addressed by export path,
with arguments crossing as one V8 blob. An export that cannot cross no longer
fails a plain run — it is reported in the new `skippedExports` instead.
