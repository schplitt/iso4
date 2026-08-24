---
"@iso4/sandbox": minor
---

fix: connection integrity and result correlation (#73)

Every `Result` is matched against the run that asked for it, and a connection
whose frame alignment is in doubt is replaced rather than reused. `maxIsolates`
is now a capacity rather than a fixed set of connections, and there is a new
host-detected error code `ERR_PROTOCOL_DESYNC`.
