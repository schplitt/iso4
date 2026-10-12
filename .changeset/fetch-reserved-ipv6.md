---
'@iso4/fetch': patch
---

fix: block IPv4-mapped and other IPv4-embedding IPv6 addresses that reach private networks

With `pinDns`, literals such as `[::ffff:127.0.0.1]` and `[::ffff:a9fe:a9fe]` were treated as public and reached loopback, private ranges and cloud metadata.
