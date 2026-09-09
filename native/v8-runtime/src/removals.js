// Deleted sandbox globals — rationale in DESIGN.md §7.5. WebAssembly returns
// later; the deny_wasm_codegen callback (v8.rs) backstops codegen meanwhile.
delete globalThis.SharedArrayBuffer
delete globalThis.WebAssembly
