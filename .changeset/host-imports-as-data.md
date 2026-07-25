---
'@iso4/sandbox': minor
'@iso4/v8-darwin-arm64': minor
'@iso4/v8-darwin-x64': minor
'@iso4/v8-linux-x64-gnu': minor
'@iso4/v8-linux-arm64-gnu': minor
---

Host-import modules are now built natively by the Rust runtime from structured shape data instead of being lowered to generated JavaScript source in the client. Previously the client walked a host-module object and emitted ESM text — data leaves printed as JS literals, function leaves as `__iso4_call(<id>, …)` stubs — an injection-adjacent codegen surface, and the runtime only ever saw opaque source. The module shape now crosses the wire as plain data (function-leaf markers, data leaves as `WireValue`s, nested objects as trees); the runtime materialises data leaves with the value codec, builds function leaves as async trampolines from a fixed factory (the handle ID passed as a number, never printed into source), and hands the values to a fixed-shape module through V8's `import.meta` callback. Host modules remain ordinary source-text modules, so prefix snapshots capture their bindings as plain JS exactly as before.

The runtime owns the handle → `<specifier>.<path>` table, so `RunResult.bridgeCalls` records now arrive with **fully resolved names** for host-import calls (e.g. `tools:search.query`) straight from the runtime — the client-side name resolver is gone — and `BridgeCall` frames carry the resolved import target (`targetKind: import` + specifier + leaf path) instead of a dispatcher name and a numeric handle argument.

Undeclared-import rebind validation moved into the runtime: `prefix.run()` sends only the rebind locations, and the runtime checks them against the shape declared at `precompile()`, rejecting undeclared specifiers/paths, data leaves, and source modules with `ERR_UNDECLARED_BINDING` — the same enforcement point that guards undeclared globals, no longer bypassable by a non-TypeScript client. Compile-time enforcement via `RebindImports<M>` is unchanged.

Wire-protocol change (`ImportBinding` is now tagged source/host with a shape tree; `PrefixRun` carries `ImportRebind` locations; `BridgeCallRecord` carries a resolved `name` and drops `rawName`/`importHandleId`): `@iso4/sandbox` and the `@iso4/v8-*` runtime binaries must be released together.
