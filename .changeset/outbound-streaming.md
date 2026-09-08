---
"@iso4/sandbox": minor
---

feat: sandbox response bodies stream back to the host (#128)

A session call may now return a `Response`/`Request` whose body is an async iterable or a passed-through `request.body`; the host receives a real `ReadableStream` whose reads drive sandbox-side production under credit-based flow control. Cancellation follows web-streams semantics (guest `finally` cleanup runs), and an abandoned stream is dropped after 10 s idle.
