---
"@iso4/sandbox": minor
---

Rename `sandbox.precompile()` → `sandbox.prepare()` and `prefix.run()` → `prefix.execute()`. The new names are the canonical API; the former names remain as **deprecated aliases** with identical behavior (they delegate to the same implementation) and are slated for removal in a future major. No behavior change — existing code keeps working. `Sandbox.prepare` and `Prefix.execute` are first-class members of the public type surface; `precompile`/`run` carry `@deprecated` JSDoc.
