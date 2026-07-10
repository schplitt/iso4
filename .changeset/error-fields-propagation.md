---
"@iso4/sandbox": patch
"@iso4/v8-darwin-arm64": patch
"@iso4/v8-darwin-x64": patch
"@iso4/v8-linux-x64-gnu": patch
"@iso4/v8-linux-arm64-gnu": patch
---

Restore thrown-error shape across the bridge: direct props in the sandbox, `error.fields` on the host.

- **Host → sandbox:** a host handler error's own-enumerable properties (e.g. `status`, `reason`, a custom `reasoning`) are now re-attached as **direct own properties** on the Error the sandbox catches — `e.status` instead of the previous `e.data.status`. Reserved keys (`name`/`message`/`stack`/`__proto__`) can never be injected through the payload. Rethrowing the caught error (or spreading it into a fresh one) round-trips all fields to the host.
- **Sandbox → host (breaking rename, pre-1.0):** `RunResult.error.data` is now `RunResult.error.fields`, typed `Record<string, unknown>` — a record of *all* extra own-enumerable props, so a thrown error's own `data` property lands as `fields.data` and nothing can collide with `error.code`. `stack` stays a dedicated top-level `stack?: string`; `name`/`message`/`stack` are reserved and never appear inside `fields`.
- **Thrown primitives fixed:** `throw "some string"` in the sandbox now produces a clean `{name: 'Error', message: 'some string'}` — previously it carried a literal `"undefined"` stack and the string's character indices as data.
