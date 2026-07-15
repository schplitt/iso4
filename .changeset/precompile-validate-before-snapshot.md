---
"@iso4/sandbox": patch
---

Fix a crash where `precompile()` with an unresolvable import (or otherwise un-instantiable prefix module) could segfault the runtime process instead of returning an error. Precompile now validates the prefix in a throwaway isolate before building the snapshot, so a bad prefix — syntax error, unresolved import (including `node:async_hooks`, which is intentionally not available in prefix code), or throwing top-level code — fails cleanly with the appropriate error (e.g. `ERR_MODULE_NOT_FOUND`).
