---
"@iso4/sandbox": patch
---

fix: `console.log` renders objects instead of `[object Object]`

Logging any non-string used to call `String(value)`, so `console.log({ user: 'bob' })` produced `[object Object]` and told you nothing. Arguments are now rendered as JSON when JSON can describe them — plain objects, arrays, class instances, and anything with a `toJSON()` — and fall back to `String(value)` when it cannot, so a `Map` still reads as `[object Map]` rather than `{}`. Errors log their stack, symbols no longer render as `[unprintable]`, and typed arrays keep their element list instead of serializing one entry per byte.

Rendering reads guest properties, so it is exception-safe: a self-referencing object or a throwing getter degrades to a placeholder and never fails the run.
