---
'@iso4/sandbox': patch
'@iso4/v8-darwin-arm64': patch
'@iso4/v8-darwin-x64': patch
'@iso4/v8-linux-x64-gnu': patch
'@iso4/v8-linux-arm64-gnu': patch
---

Exported `Uint8Array` values now arrive on the host as a real `Uint8Array` instead of an index-keyed object. Unsupported types (`Date`, `Map`, `Set`, `RegExp`, `ArrayBuffer`, typed arrays other than `Uint8Array`, class instances) now fail with a clear error in both directions instead of silently turning into `{}`. `Date` was also removed from host-module data leaves so what goes in matches what can come back out.
