---
'@iso4/sandbox': minor
'@iso4/v8-darwin-arm64': minor
'@iso4/v8-darwin-x64': minor
'@iso4/v8-linux-x64-gnu': minor
'@iso4/v8-linux-arm64-gnu': minor
---

The Rust runtime now owns the resource-limit defaults; the client sends only the limits the caller explicitly set. Previously the default numbers (64 MB memory, 5 s CPU, 30 s wall, 10 bridge calls, 16 MiB export/bridge-payload caps, 1 MiB stdio caps) were filled in client-side before every Run/Precompile/PrefixRun and shipped as concrete values on the wire, so the same constants were documented in three places (TS code, `types.ts` jsdoc, Rust doc comments) that could drift, and any non-TS client had to re-implement them to get safe behavior.

Each `ResourceLimits` field is now `Optional<u32>` on the wire: absent means "apply the runtime default", an explicit `0` still means "no limit" (distinct from absent). Rust resolves any absent field from a single set of `DEFAULT_*` constants in `native/v8-runtime/src/ipc.rs` — the source of truth. The public `ResourceLimits` fields become optional to match (`limits` is `ResourceLimits` rather than `Partial<ResourceLimits>` at the call sites), and their `@default` jsdoc now documents the runtime defaults it mirrors. Effective behavior for existing callers is unchanged; the defaults are identical.

Wire-protocol change (limits payload shape): `@iso4/sandbox` and the `@iso4/v8-*` runtime binaries must be released together.
