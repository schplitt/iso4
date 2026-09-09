---
"@iso4/sandbox": patch
"@iso4/v8-darwin-arm64": patch
"@iso4/v8-darwin-x64": patch
"@iso4/v8-linux-x64-gnu": patch
"@iso4/v8-linux-arm64-gnu": patch
---

Actually statically link libstdc++ into the linux-gnu binaries. 0.5.1's `-static-libstdc++` was a noop: it only suppresses the gcc driver's implicit stdlib, while ada-url's build script emits an explicit `-lstdc++` that still resolved to the shared library. The build now mutes that via `CXXSTDLIB` and links `libstdc++.a` directly, so the binaries no longer import `GLIBCXX_3.4.31` and start again on Debian bookworm-era images like `node:24-bookworm-slim`. The glibc floor is unchanged at 2.34.
