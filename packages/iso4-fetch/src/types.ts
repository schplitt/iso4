/**
 * \@iso4/fetch — FetchHandler types and option/policy types for hardened fetch.
 *
 * The responsibility split: `iso4` handles mechanical hygiene, this package
 * provides hardened defaults, and host application code supplies policy.
 */

// ─────────────────────────────────────────────────────────────────────────
// FetchHandler
// ─────────────────────────────────────────────────────────────────────────

/**
 * The request object a FetchHandler receives from the bridge when the sandbox
 * calls `fetch(url, init)`.
 */
export interface HostFetchRequest {
  url: string
  method: string
  /**
   * Header names are lowercased.
   */
  headers: Record<string, string>
  /**
   * `null` for bodyless methods.
   */
  body: Uint8Array | string | null
  /**
   * Planned — not yet wired from the bridge; present here so the type is
   * forward-compatible.
   */
  signal?: AbortSignal
}

export interface HostFetchResponse {
  status: number
  statusText?: string
  headers: Record<string, string>
  /**
   * Any V8-serializable value — the bridge carries it as plain data.
   * Use `Uint8Array` for raw bytes, `string` for text, a plain object or
   * array for parsed JSON, or `null` for bodyless responses.
   */
  body: unknown
}

/**
 * The internal bridge handler produced by `createSafeFetch`. Validates raw
 * bridge args, runs allow/deny, makes the HTTP call, and returns plain data.
 *
 * Returned as the `handler` field of `BridgeWithShim` — not meant to be
 * used directly. Assign `createSafeFetch({...})` to a global and the sandbox
 * layer handles wiring.
 */
export type SafeFetchFn = (...args: unknown[]) => Promise<HostFetchResponse>

/**
 * A host-side request passed to {@link SafeFetchInvoke}. Mirrors the web
 * `fetch(url, init)` shape — `method`, `headers`, and `body` are optional and
 * default the same way the bridge path defaults them.
 */
export interface SafeFetchInvokeRequest {
  url: string
  /**
   * HTTP method. Case-insensitive; defaults to `GET`.
   */
  method?: string
  /**
   * Request headers. Names are lowercased; a `host` header is stripped.
   */
  headers?: Record<string, string>
  /**
   * Request body. `null`/omitted for bodyless methods.
   */
  body?: Uint8Array | string | null
  /**
   * Optional abort signal for this request.
   */
  signal?: AbortSignal
}

/**
 * Host-side per-call entry point. Runs the same allow/deny + middleware
 * pipeline as the bridge {@link SafeFetchFn}, but takes a structured request
 * and a typed per-invocation `context` that is surfaced on
 * {@link FetchContext} (`ctx.context`) for middleware to read.
 *
 * The `context` is strictly per-call — concurrent invocations never observe
 * each other's context.
 */
export type SafeFetchInvoke<TCtx = undefined> = (
  request: SafeFetchInvokeRequest,
  context: TCtx,
) => Promise<HostFetchResponse>

/**
 * What `createSafeFetch` returns. Conforms to `BridgeWithShim` from
 * `@iso4/sandbox` so it can be passed directly as a global value.
 *
 * Declared locally to avoid a cross-package type dependency — structurally
 * compatible with the sandbox type.
 *
 * `TCtx` is the type of the per-call context threaded through {@link invoke}
 * onto {@link FetchContext} (`ctx.context`).
 */
export interface SafeFetchGlobal<TCtx = undefined> {
  kind: 'bridge-with-shim'
  handler: SafeFetchFn
  shim: string
  /**
   * Host-side per-call entry. Invoke the hardened fetch pipeline directly
   * (bypassing the sandbox bridge) with a typed per-invocation `context`
   * readable from every middleware level via `ctx.context`.
   *
   * ```ts
   * await safeFetch.invoke({ url, method, headers, body }, ctx)
   * ```
   */
  invoke: SafeFetchInvoke<TCtx>
}

// ─────────────────────────────────────────────────────────────────────────
// Middleware
// ─────────────────────────────────────────────────────────────────────────

/**
 * The mutable request side of the middleware context.
 *
 * All mutations are invisible to the sandbox agent — they only affect what
 * goes on the wire. `raw` preserves the original unmodified request from
 * the bridge at all times.
 */
export interface FetchContextReq {
  /**
   * Current URL (starts as the agent's URL; updated by `setUrl`).
   */
  readonly url: string
  /**
   * HTTP method, uppercased.
   */
  readonly method: string
  /**
   * Route parameters extracted by the path pattern.
   * e.g. `/users/:id` matching `/users/42` → `{ id: '42' }`.
   */
  readonly params: Record<string, string>
  /**
   * Redirect hop index — 0 for the initial request.
   */
  readonly hop: number
  /**
   * Original untouched request from the bridge.
   */
  readonly raw: HostFetchRequest

  /**
   * Outgoing headers. Mutate directly or use `header()` for automatic
   * lowercasing. These replace whatever the agent sent.
   */
  headers: Record<string, string>

  /**
   * Outgoing body. Mutate directly or use `setBody()`.
   */
  body: Uint8Array | string | null

  /**
   * Set (or overwrite) a single outgoing header.
   * The name is lowercased automatically.
   */
  header: (name: string, value: string) => void

  /**
   * Replace the outgoing URL.
   */
  setUrl: (url: string) => void

  /**
   * Replace the outgoing body.
   */
  setBody: (body: Uint8Array | string | null) => void
}

/**
 * Context passed to every `FetchMiddleware`.
 *
 * `req` is the mutable outgoing request (mutate before `next()`).
 * `res` is the response being built (`null` until `next()` completes or a
 * middleware returns a response directly). Read and reassign after `next()`
 * to inspect or replace the response.
 *
 * `TCtx` is the per-invocation context type supplied by the host caller via
 * {@link SafeFetchGlobal.invoke}.
 */
export interface FetchContext<TCtx = undefined> {
  readonly req: FetchContextReq
  /**
   * The response. `null` before `next()` is called.
   * After `next()`, holds the response from downstream.
   * Assign to replace it:
   * ```ts
   * await next()
   * ctx.res = { ...ctx.res!, body: parsed }
   * ```
   */
  res: HostFetchResponse | null
  /**
   * Per-invocation context supplied by the host caller through
   * {@link SafeFetchGlobal.invoke}. `undefined` on the sandbox-driven bridge
   * path (there is no host caller to supply it).
   */
  readonly context: TCtx
}

/**
 * A middleware function that runs after the allow/deny check and before the
 * real HTTP call. Mutations to `ctx.req` are invisible to the agent but
 * applied to every outgoing request that matches the level the middleware
 * is attached to.
 *
 * Three levels of middleware are available — global (on `createSafeFetch`),
 * origin (on `FetchOriginRule`), and route (on `FetchRouteRule`). They run
 * in that order so more specific middleware runs last and can override
 * broader changes.
 *
 * @example inject auth (covers every case: auth, logging, URL rewriting)
 * ```ts
 * async (ctx) => {
 *   ctx.req.header('authorization', `Bearer ${await vault.get('key')}`)
 *   console.log(`→ ${ctx.req.method} ${ctx.req.url}`)
 * }
 * ```
 */
/**
 * Runs the downstream middleware chain and ultimately the real HTTP call.
 * Sets `ctx.res` when it completes. Returns `void` — read `ctx.res` after
 * awaiting to inspect the response.
 */
export type Next = () => Promise<void>

/**
 * A middleware function in the `(ctx, next)` style.
 *
 * **Return value semantics:**
 * - Return `void` / nothing — chain continues; `ctx.res` propagates as-is.
 * - Return a `HostFetchResponse` — that response is set on `ctx.res` and
 *   returned to the agent immediately. Outer middleware still runs its
 *   after-`next()` code but the HTTP call is skipped.
 * - `throw` — execution terminates; the sandbox `fetch()` call rejects.
 *   Use this to block a request with an error visible to the agent.
 *
 * **Pattern: mutate request (auth, URL rewrite)**
 * ```ts
 * async (ctx, next) => {
 *   ctx.req.header('authorization', `Bearer ${await vault.get('key')}`)
 *   await next()
 * }
 * ```
 *
 * **Pattern: inspect / rewrite response**
 * ```ts
 * async (ctx, next) => {
 *   await next()
 *   ctx.res = { ...ctx.res!, body: JSON.parse(new TextDecoder().decode(ctx.res!.body as Uint8Array)) }
 * }
 * ```
 *
 * **Pattern: log timing**
 * ```ts
 * async (ctx, next) => {
 *   const t = Date.now()
 *   await next()
 *   console.log(`${ctx.req.method} ${ctx.req.url} → ${ctx.res?.status} (${Date.now() - t}ms)`)
 * }
 * ```
 *
 * **Pattern: synthetic response (no HTTP)**
 * ```ts
 * async (ctx, _next) => ({
 *   status: 202,
 *   headers: { 'content-type': 'application/json' },
 *   body: { queued: true, id: ctx.req.params['id'] },
 * })
 * ```
 *
 * **Pattern: block with error**
 * ```ts
 * async (ctx, next) => {
 *   if (isSuspicious(ctx.req)) throw new Error('request blocked')
 *   await next()
 * }
 * ```
 */
export type FetchMiddleware<TCtx = undefined> = (
  ctx: FetchContext<TCtx>,
  next: Next,
) => HostFetchResponse | void | Promise<HostFetchResponse | void>

// ─────────────────────────────────────────────────────────────────────────
// Route-based allowlist
// ─────────────────────────────────────────────────────────────────────────

/**
 * A single route entry within a `FetchOriginRule`.
 *
 * Path patterns use URLPattern/rou3 syntax:
 *   `/users/:id`            — named parameter
 *   `/v2/events`            — exact match
 *   `/reports/**`           — zero-or-more sub-path segments
 *   `/files/:ext(png|jpg)`  — constrained parameter
 */
export interface FetchRouteRule<TCtx = undefined> {
  /**
   * Path pattern. Must start with `/`.
   * `/**` and `/prefix/**` both match the prefix itself (zero-segment wildcard).
   */
  path: string

  /**
   * Allowed HTTP method(s). Case-insensitive.
   * A string, an array, `'*'`, or omitted all mean "any method".
   */
  methods?: string | string[]

  /**
   * Middleware that runs for requests matching this specific route,
   * after global and origin middleware. Route middleware runs last so it
   * can override anything set at broader levels.
   */
  middleware?: FetchMiddleware<TCtx>
}

/**
 * Per-origin set of allowed routes.
 */
export interface FetchOriginRule<TCtx = undefined> {
  /**
   * Hostname(s) to match.
   * - Exact: `'api.example.com'`
   * - Single-level subdomain wildcard: `'*.example.com'`
   *   (matches `sub.example.com`, not `a.b.example.com` or `example.com`)
   * - Array for multiple hostnames under the same routes.
   */
  host: string | string[]

  /**
   * Allowed port(s). When omitted, only the standard port for the protocol
   * is allowed (443 for https, 80 for http).
   */
  port?: number | number[]

  /**
   * Require HTTPS. Default `true`. Must be explicitly `false` to allow HTTP.
   *
   * @default true
   */
  httpsOnly?: boolean

  /**
   * Route rules for this origin. Method + path must match at least one entry.
   * First match wins. An empty array denies all paths on this origin.
   */
  routes: FetchRouteRule<TCtx>[]

  /**
   * Middleware that runs for every matched request on this origin, after
   * global middleware and before route middleware.
   */
  middleware?: FetchMiddleware<TCtx>
}

// ─────────────────────────────────────────────────────────────────────────
// Policy
// ─────────────────────────────────────────────────────────────────────────

export interface SafeFetchRequest {
  /**
   * Full canonical URL string.
   */
  url: string
  /**
   * Protocol without trailing colon — always `'http'` or `'https'`.
   */
  protocol: 'http' | 'https'
  /**
   * Hostname only, lowercased.
   */
  host: string
  /**
   * Effective port — 443 or 80 when not explicit in the URL.
   */
  port: number
  /**
   * Path plus query string, starting with `/`.
   */
  path: string
  /**
   * Uppercased HTTP method.
   */
  method: string
  /**
   * Request headers, names lowercased.
   */
  headers: Record<string, string>
  /**
   * Resolved IP when `pinDns: true`. `null` when `pinDns: false` or when
   * using policy-only mode without rules.
   */
  resolvedIp: string | null
  /**
   * `0` for the initial request, `1+` for redirect hops.
   */
  hop: number
}

/**
 * Allow/deny predicate for a single request.
 *
 * - Return `true` to allow.
 * - Return `false` to deny with a generic message.
 * - `throw new Error('reason')` to deny with a custom message visible to
 *   sandbox code.
 * - Async (returning a `Promise`) is supported.
 */
export type SafeFetchPolicy = (
  request: SafeFetchRequest,
) => boolean | Promise<boolean>

// ─────────────────────────────────────────────────────────────────────────
// createSafeFetch options
// ─────────────────────────────────────────────────────────────────────────

export interface SafeFetchOptions<TCtx = undefined> {
  /**
   * Declarative origin + route allowlist. At least one of `rules` or
   * `policy` is required.
   *
   * If an origin matches but no route does → DENY (does not fall through to
   * `policy`). If no origin matches → falls through to `policy` if provided.
   */
  rules?: FetchOriginRule<TCtx> | FetchOriginRule<TCtx>[]

  /**
   * Allow/deny callback — fallback for requests not matched by any rule, or
   * the sole mechanism when `rules` is omitted.
   */
  policy?: SafeFetchPolicy

  /**
   * Global middleware that runs for every allowed request, before origin and
   * route middleware.
   */
  middleware?: FetchMiddleware<TCtx>

  /**
   * Pre-resolve DNS and pin the connection to the resolved IP.
   * Prevents SSRF and DNS rebinding. `SafeFetchRequest.resolvedIp` is set.
   *
   * @default true
   */
  pinDns?: boolean

  /**
   * Auto-follow redirects up to this many hops. The full allow/deny +
   * middleware chain re-runs on each hop. `0` passes 3xx responses through.
   *
   * @default 0
   */
  maxRedirects?: number

  /**
   * Per-request timeout in milliseconds.
   *
   * @default 30_000
   */
  timeoutMs?: number

  /**
   * Maximum response body size in bytes (streaming, enforced before the
   * response reaches any caller).
   *
   * @default 16 * 1024 * 1024
   */
  maxBodyBytes?: number

  /**
   * Allow compressed responses. When `false`, `Accept-Encoding: identity`
   * is sent to prevent response-amplification attacks.
   *
   * @default false
   */
  allowCompressedResponses?: boolean

  /**
   * Alert hook — called on every denial. Errors thrown here are ignored.
   * For request logging use middleware with `next()`.
   */
  onDenied?: (request: SafeFetchRequest, reason: string) => void | Promise<void>
}
