---
"@iso4/fetch": patch
---

fix: match request paths literally instead of decoding them

The route allowlist now matches the path exactly as it is sent, with no
percent-decoding, so an encoded slash can no longer make the matched path differ
from the path on the wire. `.`/`..` are still normalised by the URL parser.
