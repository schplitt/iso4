---
"@iso4/fetch": minor
---

Add typed, per-call context for `@iso4/fetch` middleware.

`createSafeFetch<TCtx>()` is now generic; the type flows into
`FetchContext<TCtx>.context` and `FetchMiddleware<TCtx>`. The returned
`SafeFetchGlobal<TCtx>` gains an `invoke(request, context)` entry that runs the
same allow/deny + middleware pipeline host-side while threading a per-invocation
context onto `ctx.context`, readable from all three middleware levels
(global/origin/route).

The context is strictly per-call — concurrent invocations never observe each
other's context. Fully backward compatible: existing `createSafeFetch({...})`
usage compiles unchanged, and the sandbox bridge path leaves `ctx.context`
`undefined`.
