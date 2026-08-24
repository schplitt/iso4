---
"@iso4/sandbox": patch
---

fix: clean up the runtime process when `createSandbox()` fails

A failed startup left the runtime running with no `Sandbox` to dispose it. A
runtime that exits during startup is now reported with its exit code instead
of a socket timeout.
