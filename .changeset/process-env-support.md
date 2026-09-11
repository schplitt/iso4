---
"@iso4/sandbox": minor
---

feat: `process.env` support — an `env` option on prepare/run and a curated `process` global

Sandbox code now sees a lazily installed `process` global (importable as `node:process`) with a writable per-run `process.env` snapshot declared via the new `env` option on `prepare()`, `execute()`, `call()`, and `run()`. `process.exit()` fails the run with the new `ERR_PROCESS_EXIT` code, and `process` is now a reserved global name.
