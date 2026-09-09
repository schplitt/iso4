---
"@iso4/sandbox": patch
---

feat: remove WebAssembly from the sandbox

The `WebAssembly` global is deleted from every context, and wasm code generation is denied isolate-wide underneath. Sandbox code should feature-detect with `typeof WebAssembly === 'undefined'`, like in a browser without wasm.
