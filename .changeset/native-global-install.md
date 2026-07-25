---
'@iso4/sandbox': minor
'@iso4/v8-darwin-arm64': minor
'@iso4/v8-darwin-x64': minor
'@iso4/v8-linux-x64-gnu': minor
'@iso4/v8-linux-arm64-gnu': minor
---

Host globals are now installed natively by the Rust runtime instead of by prepending generated JavaScript to the user's code. Previously `processGlobals` turned string globals and `BridgeWithShim` wrappers into source text and pasted it in front of the run's code, which shifted every line of user code so sandbox stack traces pointed at the wrong lines, and interpolated the global's name into an identifier position in the generated wrapper.

The client now sends each global as structured data — a `GlobalDef` tagged `bridge`, `string`, `data`, or `shim` — and the runtime installs it directly on the sandbox global object via the V8 API. String expressions and shim wrappers are evaluated as their own scripts with their own filenames, so **user code always starts at line 1 and its stack traces are correct**, and a global's name only ever travels as a string passed to `object.set` (or a `WireValue`) — never interpolated into code.

Adds a new **data-valued global kind** for passing a plain constant: `globals: { config: { kind: 'data', value: … } }`. The value crosses the wire as a `WireValue` (same supported set as host-module data leaves — primitives, `bigint`, `string`, `Uint8Array`, plain objects/arrays) and is materialised natively, removing the need to hand-roll a `JSON.stringify`-into-a-string-expression global. Data globals, like string globals, are constants and cannot be rebound per `prefix.run()`.

For precompiled prefixes, string/data globals and shim wrappers are baked into the snapshot at `precompile()` time exactly as before, so only bridge stubs are re-installed per `prefix.run()` — the repeated-run hot path is unchanged.

Wire-protocol change (the `globals` field of `Run`/`Precompile`/`PrefixRun` is now a `List<GlobalDef>` instead of a name list): `@iso4/sandbox` and the `@iso4/v8-*` runtime binaries must be released together.
