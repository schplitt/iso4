---
'@iso4/sandbox': patch
---

fix(sandbox): stop logging a connection error on every `sandbox.dispose()`

The host now sends a close frame on orderly shutdown, so only connections that end without one (or mid-frame) are logged.
