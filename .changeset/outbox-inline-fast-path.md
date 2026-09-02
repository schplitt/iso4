---
"@iso4/sandbox": patch
"@iso4/v8-darwin-arm64": patch
"@iso4/v8-darwin-x64": patch
"@iso4/v8-linux-arm64-gnu": patch
"@iso4/v8-linux-x64-gnu": patch
---

perf: outbound frames skip the writer thread when the socket is free (#127)

Removes the flat per-call overhead the bounded outbound queue introduced
for short, frequent calls; the writer thread still takes over whenever the
socket backs up, so stall isolation is unchanged.
