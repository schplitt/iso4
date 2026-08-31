---
"@iso4/sandbox": minor
"@iso4/v8-darwin-arm64": minor
"@iso4/v8-darwin-x64": minor
"@iso4/v8-linux-x64-gnu": minor
"@iso4/v8-linux-arm64-gnu": minor
---

Freeze the sandbox clock during execution, workerd-style. `Date.now()`, no-arg `new Date()`/`Date()`, no-arg `Intl.DateTimeFormat` formatting, and `Temporal.Now.*` all read one per-context value that advances — monotone, whole milliseconds — only when the runtime regains control at run entry, a bridge response, or a stream frame. Sandboxed code can no longer observe its own elapsed execution time, closing the timing side-channel between co-resident isolates. Explicit-argument `Date`/`Temporal`/`Intl` computation is untouched. Alongside it, `SharedArrayBuffer` is removed from the sandbox global (as in non-cross-origin-isolated browsers; `Atomics` on plain buffers keeps working) and `Atomics.wait` now throws, matching workerd.
