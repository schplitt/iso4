---
"@iso4/sandbox": patch
---

fix(sandbox): lift the 32-level nesting cap on host → sandbox values

Host values may now nest to any depth V8 can serialize, and cycles, shared references and `Map`/`Set` entries holding a `Response`, `Request` or `Headers` rehydrate correctly. A host value too deep for the runtime to read fails the run with `ERR_TYPE_NOT_SERIALIZABLE` instead of `ERR_INTERNAL`.
