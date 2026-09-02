---
"@iso4/sandbox": patch
"@iso4/v8-darwin-arm64": patch
"@iso4/v8-darwin-x64": patch
"@iso4/v8-linux-arm64-gnu": patch
"@iso4/v8-linux-x64-gnu": patch
---

fix: a host that stops draining a connection can no longer stall sandbox execution (#127)

Outbound frames now go through a bounded per-connection queue with a
dedicated writer thread; a peer that stops reading fails that connection's
runs cleanly after a bounded wait instead of freezing every instance that
shares it.
