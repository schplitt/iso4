---
"@iso4/sandbox": patch
"@iso4/v8-darwin-arm64": patch
"@iso4/v8-darwin-x64": patch
"@iso4/v8-linux-x64-gnu": patch
"@iso4/v8-linux-arm64-gnu": patch
---

Statically link libstdc++ into the linux-gnu binaries. 0.5.0 dynamically linked libstdc++ and imported a `GLIBCXX_3.4.31` symbol (via ada-url's C++ built with GCC 13 on the release runner), so the binary refused to start on Debian bookworm-era images like `node:24-bookworm-slim`. The glibc floor is unchanged at 2.34.
