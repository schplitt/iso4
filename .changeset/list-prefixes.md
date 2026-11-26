---
"@iso4/sandbox": patch
---

feat(native): the runtime answers `ListPrefixes` from a supervisor-role connection with the prefixes it holds

Each row carries the prefix id, code hash, live or broken state and heap limits; a node-role connection is refused with `ERR_INTERNAL` and stays open. Node never sends the frame, so nothing changes for callers.
