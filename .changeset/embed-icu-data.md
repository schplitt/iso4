---
"@iso4/sandbox": patch
"@iso4/v8-darwin-arm64": patch
"@iso4/v8-darwin-x64": patch
"@iso4/v8-linux-x64-gnu": patch
"@iso4/v8-linux-arm64-gnu": patch
---

Embed ICU data in the runtime binary. Locale-aware calls in sandboxed code (`toLocaleString`, `Intl.*`, `localeCompare`) previously aborted the whole V8 runtime process with "Fatal process out of memory: DateTimePatternGeneratorCache::CreateGenerator", leaving the sandbox unreachable for every subsequent run. They now return correctly localized output.
