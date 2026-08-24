---
"@iso4/sandbox": minor
---

feat: widen `HostExportData` to everything V8 serialization carries

`Date`, `RegExp`, `Error`, `Map`, `Set`, `ArrayBuffer`, typed arrays, `DataView`
and cycles now cross as real instances. Host-module data leaves are no longer
inspected at registration, so an unsupported value fails with the serializer's
own error rather than one naming the exact leaf path.
