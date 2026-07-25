---
'@iso4/sandbox': patch
'@iso4/v8-darwin-arm64': patch
'@iso4/v8-darwin-x64': patch
'@iso4/v8-linux-x64-gnu': patch
'@iso4/v8-linux-arm64-gnu': patch
---

Bridge limit violations (`maxBridgeCalls`, `maxBridgeCallBytes`, function arguments) now terminate V8 execution immediately and uncatchably, as DESIGN.md always specified. Previously they were thrown as catchable JS exceptions: sandbox code could `try/catch` past a violation in the synchronous window before the next microtask checkpoint, keep making (blocked) call attempts, or even complete the run successfully despite the violation. Host handler errors are unchanged and remain catchable in the sandbox.
