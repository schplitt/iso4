# @iso4/fetch

Secure HTTP plumbing for host-authored APIs exposed to [@iso4/sandbox](../iso4-sandbox) agents.

## The pattern

The sandbox bridge only carries plain data — the agent can never receive a
real `Response` object with `.json()` or `.text()`. Exposing a raw `fetch`
global is therefore the wrong shape.

The right pattern: author a typed host module with a domain-specific API
and wire it to real HTTP internally. The agent learns the API from TypeScript
types you provide and never touches raw fetch.

```ts
// agent code — clean, typed by the host
import { getUsers, queueDeletion } from 'host:api'

const users = await getUsers() // res.body is a ready-to-use array
const ack = await queueDeletion(42) // no real DELETE was sent
```

```ts
// host code
import { createSafeFetch } from '@iso4/fetch'

const fetch = createSafeFetch({
  rules: {
    host: 'api.example.com',
    // all routes on this origin get auth injected automatically
    middleware: async (ctx, next) => {
      ctx.req.header('authorization', `Bearer ${await vault.get('token')}`)
      return next()
    },
    routes: [
      {
        path: '/v1/users/**',
        methods: 'GET',
        // real HTTP + unwrap the response for the agent
        middleware: async (_ctx, next) => {
          const res = await next()
          return await res.json() // agent gets res.body as a parsed object
        },
      },
      {
        path: '/v1/items/:id',
        methods: 'DELETE',
        // no HTTP — synthesise the response entirely
        middleware: async (ctx, _next) => ({
          status: 202,
          headers: { 'content-type': 'application/json' },
          body: { queued: true, id: ctx.req.params['id'] },
        }),
      },
    ],
  },
})
```

## Install

```sh
npm i @iso4/sandbox @iso4/fetch
```

## Rules

Every request must match a declared rule before any middleware runs or any
network call is made. Unmatched requests are denied immediately.

```ts
const fetch = createSafeFetch({
  rules: [
    {
      host: 'api.github.com', // exact hostname or '*.example.com' (single level)
      httpsOnly: true, // default — set false to allow HTTP
      // port: 443                 // default; list explicit ports to allow others
      routes: [
        { path: '/repos/**', methods: ['GET', 'POST'] },
        { path: '/repos/:owner/:repo/issues/:id', methods: 'GET' },
      ],
    },
    {
      host: 'raw.githubusercontent.com',
      routes: [{ path: '/**', methods: 'GET' }],
    },
  ],
})
```

Path patterns use [rou3](https://github.com/h3js/rou3) / URLPattern syntax:

| Pattern                 | Matches                                                        |
| ----------------------- | -------------------------------------------------------------- |
| `/users/:id`            | `/users/42` — named param, available as `ctx.req.params.id`    |
| `/api/**`               | `/api`, `/api/users`, `/api/users/123` — zero-or-more segments |
| `/files/:ext(png\|jpg)` | `/files/png` — constrained param                               |

An empty `routes: []` denies all paths on that origin.

## Middleware

Middleware is the single extension point. The `(ctx, next)` pattern covers
every use case: request mutation, response inspection, logging, and synthetic
overrides.

Three levels, running in order — global wraps origin wraps route:

```ts
createSafeFetch({
  middleware: someGlobalMiddleware, // global — every request
  rules: {
    host: 'api.example.com',
    middleware: someOriginMiddleware, // origin — every request to this host
    routes: [{
      path: '/v1/**',
      middleware: someRouteMiddleware, // route — this path only
    }],
  },
})
```

### Request mutation (auth, URL rewrite, headers)

```ts
async function someRequestMiddleware(ctx, next) {
  ctx.req.header('authorization', `Bearer ${token}`) // injected, agent never sees it
  ctx.req.setUrl(ctx.req.url.replace('/v1/', '/v2/'))
  return next()
}
```

`ctx.req` fields: `url` (via `setUrl()`), `method`, `headers` (mutable object
or `header(name, val)`), `body` (via `setBody()`), `params`, `hop`, `raw`
(original unmodified bridge request).

### Response inspection / logging

```ts
async function someResponseMiddleware(ctx, next) {
  const t = Date.now()
  const res = await next()
  console.log(`${ctx.req.method} ${ctx.req.url} → ${res.status} (${Date.now() - t}ms)`)
  return res
}
```

### Response rewrite (replaces `transform`)

```ts
async function someResponseRewriteMiddleware(_ctx, next) {
  const res = await next()
  const data = JSON.parse(new TextDecoder().decode(res.body as Uint8Array))
  return { ...res, body: data } // agent gets res.body as a parsed object
}
```

### Synthetic response — no HTTP (replaces `handle`)

Skip `next()` entirely. No network call is made.

```ts
async function someSyntheticResponseMiddleware(ctx, _next) {
  return {
    status: 202,
    headers: { 'content-type': 'application/json' },
    body: { queued: true, id: ctx.req.params['id'] },
  }
}
```

## Policy callback

For dynamic allow/deny logic that can't be expressed as static rules —
per-tenant checks, rate limiting, etc. Falls back to this when no origin rule
matches, or is the sole mechanism when `rules` is omitted.

```ts
createSafeFetch({
  policy: async ({ host, method, resolvedIp }) => {
    if (resolvedIp && isInternal(resolvedIp))
return false
    const tenant = await db.findByHost(host)
    return tenant?.allowedMethods.includes(method) ?? false
  },
})
```

`policy` runs only when no `rules` origin matched. If an origin matches but
no route does the request is denied without consulting `policy`.

## Security defaults

| Threat                 | Mitigation                                                                   |
| ---------------------- | ---------------------------------------------------------------------------- |
| SSRF / private IP      | DNS pre-resolved before every request; loopback, RFC1918, link-local blocked |
| DNS rebinding          | undici DNS interceptor pins the connection to the resolved IP                |
| Redirect bypass        | No auto-follow by default; allow/deny re-checked on each hop                 |
| Response amplification | Body streamed with `maxBodyBytes` cap                                        |
| Host auth leakage      | Isolated undici `Agent` — no shared pool, cookies, or auth with the host app |
| Path traversal         | Paths decoded and `.`/`..`-normalised before route matching                  |
| Double-encoded paths   | Detected and rejected at parse time                                          |

## License

MIT
