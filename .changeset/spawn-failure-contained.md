---
"@iso4/sandbox": patch
"@iso4/v8-darwin-arm64": patch
"@iso4/v8-darwin-x64": patch
"@iso4/v8-linux-arm64-gnu": patch
"@iso4/v8-linux-x64-gnu": patch
---

fix: an instance thread that fails to spawn fails only its own run (#127)

Under process resource exhaustion the affected run now reports
ERR_INTERNAL and the connection keeps serving, instead of the runtime
panicking the connection's demux.
