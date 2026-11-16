---
"@iso4/sandbox": patch
---

fix(native): fail more heap-limit blow-ups with `ERR_MEMORY_LIMIT` instead of killing the runtime

A run that reaches its heap limit now fails with `ERR_MEMORY_LIMIT` in more cases instead of ending the runtime child, and a run that ends a turn with its heap over its hard memory line fails the same way. Some single allocations far past the limit still end the runtime child.
