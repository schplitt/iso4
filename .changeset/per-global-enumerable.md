---
"@iso4/sandbox": patch
---

feat: per-global opt-out of enumerability

The object global forms accept `enumerable: false` to keep an injected global out of `for...in` / `Object.keys` while staying callable, and new `{ kind: 'bridge', handler }` / `{ kind: 'string', expr }` object forms carry the option for what the shorthands declare. Shorthand globals stay enumerable.
