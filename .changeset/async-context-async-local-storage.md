---
"@iso4/sandbox": minor
---

Add async context propagation inside the sandbox via a minimal, Node-compatible `AsyncLocalStorage`, imported the standard way:

```js
import { AsyncLocalStorage } from 'node:async_hooks'
const als = new AsyncLocalStorage()
await als.run(store, async () => { /* ... */ als.getStore() })
```

Sandboxed run/postfix code can now carry an ambient value across `await` points — a trace id, a durable-workflow step key — without threading it through every call, and concurrent async chains stay isolated (a module-level variable gets this wrong). The canonical use case is a `step.do(name, fn)` shim whose nested-step key is built from an `AsyncLocalStorage`, so a step nested inside another produces `parent/child` and never collides.

- Only `run(store, callback, ...args)` and `getStore()` are implemented — the concurrency-safe core.
- Built on V8's continuation-preserved embedder data (the same primitive modern Node's `AsyncContextFrame` uses). No promise hooks are registered, so runs that never use it pay nothing; when used, the small per-`await` cost bills to `cpuTimeMs`/`wallTimeMs`, never to the bridge-call budget.
- Always available to run code with no host opt-in, like `console`. **Not** available in `precompile()` (prefix) code — the native bindings can't be captured in a V8 startup snapshot. See DESIGN.md §16.
- `node:async_hooks` is the sole runtime-provided `node:*` module; a host-declared import of the same specifier takes precedence.

The `@iso4/v8-*` native binaries and `@iso4/sandbox` are version-fixed and release together.
