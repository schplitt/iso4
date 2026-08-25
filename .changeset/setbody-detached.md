---
"@iso4/fetch": patch
---

fix: `setBody` no longer depends on `this`

`ctx.req.setBody` can now be destructured or passed as a callback without
throwing; it writes the request body through a closure like `header` and
`setUrl`.
