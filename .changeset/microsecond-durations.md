---
'@iso4/sandbox': patch
'@iso4/v8-darwin-arm64': patch
'@iso4/v8-darwin-x64': patch
'@iso4/v8-linux-x64-gnu': patch
'@iso4/v8-linux-arm64-gnu': patch
---

`result.durationMs` now has microsecond resolution (e.g. `0.347`) instead of being truncated to whole milliseconds. Sub-millisecond runs previously reported `0`.
