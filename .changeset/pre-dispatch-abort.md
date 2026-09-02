---
"@iso4/sandbox": patch
"@iso4/v8-darwin-arm64": patch
"@iso4/v8-darwin-x64": patch
"@iso4/v8-linux-arm64-gnu": patch
"@iso4/v8-linux-x64-gnu": patch
---

fix: an abort that arrives before its run starts still lands gracefully (#127)

A Terminate (or connection loss) racing ahead of the run's dispatch is now
remembered and answered when the run arrives, instead of falling back to
the host-side teardown timeout.
