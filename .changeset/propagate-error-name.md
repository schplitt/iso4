---
"@iso4/sandbox": minor
---

feat: propagate JS error name and structured data across the sandbox bridge

`RunError.name` now reflects the actual JS error name (`TypeError`, `RangeError`, custom names via `error.name = '…'`) instead of always being `'Error'`. For `ERR_USER_CODE` errors, a new optional `data` field carries the thrown error's own enumerable properties (excluding `name`, `message`, `stack`) serialised via the same WireValue path as exports — functions and other non-serialisable values are silently dropped.

Non-object throws (`throw 'string'`, `throw 42`) keep `name: 'Error'` and `data: undefined`.
