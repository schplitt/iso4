# @iso4/v8-darwin-arm64

## 0.1.0

### Minor Changes

- 91fc138: feat: propagate JS error name and structured data across the sandbox bridge

  `RunError.name` now reflects the actual JS error name (`TypeError`, `RangeError`, custom names via `error.name = '…'`) instead of always being `'Error'`. For `ERR_USER_CODE` errors, a new optional `data` field carries the thrown error's own enumerable properties (excluding `name`, `message`, `stack`) serialised via the same WireValue path as exports — functions and other non-serialisable values are silently dropped.

  Non-object throws (`throw 'string'`, `throw 42`) keep `name: 'Error'` and `data: undefined`.

## 0.0.4

### Patch Changes

- fbe4332: feat: import support

## 0.0.3

### Patch Changes

- 7c943af: chore: add bin field to native packages

## 0.0.2

### Patch Changes

- 58c27ad: ci: set execute bit on native binaries

## 0.0.1

### Patch Changes

- 88554a4: initial release
