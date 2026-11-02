---
"@iso4/sandbox": patch
---

fix(sandbox): bridge call records count the serialized values only

`argBytes` and `responseBytes`, and the `maxBridgeCallBytes` check, now measure the args and response blobs alone, no longer the frame header around them (a few dozen bytes per call).
