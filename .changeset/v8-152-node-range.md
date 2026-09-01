---
"@iso4/sandbox": minor
"@iso4/v8-darwin-arm64": minor
"@iso4/v8-darwin-x64": minor
"@iso4/v8-linux-arm64-gnu": minor
"@iso4/v8-linux-x64-gnu": minor
---

feat: upgrade to V8 15.2 and span Node 22–27 with one binary (#80)

The runtime picks up five V8 release lines of security fixes and performance
work. Serialized values keep the format every shipping Node reads, while the
handshake now also accepts hosts on newer Node lines that write V8's
next serialization format — so Node 22 through 27 pair with the same binary.
