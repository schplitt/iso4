---
"@iso4/sandbox": patch
"@iso4/v8-darwin-arm64": patch
"@iso4/v8-darwin-x64": patch
"@iso4/v8-linux-arm64-gnu": patch
"@iso4/v8-linux-x64-gnu": patch
---

fix: inbound frames are read against the flat protocol ceiling (#127)

Per-run frame allowances are still enforced per run, but the connection's
read ceiling is now a constant — it can no longer shrink under an in-flight
frame, so a large late frame for a just-completed run is discarded instead
of costing the connection.
