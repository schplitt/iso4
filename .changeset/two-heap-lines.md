---
"@iso4/sandbox": minor
---

feat: retire a grown warm instance instead of killing a run (#169)

A prefix's `memoryMb` is now the line at which a reused instance is retired —
no run fails for it — and the terminating cap sits a headroom band above it
(`128` terminates at 160); pass `{ soft, hard }` to set both lines yourself, or
`{ hard }` alone for the old single-cap behavior. One-off `run()` is unchanged:
`limits.memoryMb` terminates exactly, since a fresh isolate is never reused.
