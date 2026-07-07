/**
 * \@iso4/fetch — public entry point.
 * See `./types.ts` for the full option surface.
 */

import { createRouter, addRoute, findRoute } from 'rou3'
import type { RouterContext } from 'rou3'
import { fetch as undiciFetch, Agent, interceptors } from 'undici'
import type { Dispatcher } from 'undici'
import { lookup as nodeDnsLookup } from 'node:dns/promises'
import { lookup as nodeDnsLookupCb } from 'node:dns'

import type {
  FetchContext,
  FetchContextReq,
  FetchMiddleware,
  FetchOriginRule,
  FetchRouteRule,
  HostFetchRequest,
  HostFetchResponse,
  SafeFetchFn,
  SafeFetchGlobal,
  SafeFetchInvoke,
  SafeFetchOptions,
  SafeFetchRequest,
} from './types.js'

export type {
  FetchContext,
  FetchContextReq,
  FetchMiddleware,
  FetchOriginRule,
  FetchRouteRule,
  HostFetchRequest,
  HostFetchResponse,
  Next,
  SafeFetchFn,
  SafeFetchGlobal,
  SafeFetchInvoke,
  SafeFetchInvokeRequest,
  SafeFetchOptions,
  SafeFetchPolicy,
  SafeFetchRequest,
} from './types.js'

// ─────────────────────────────────────────────────────────────────────────
// IP address utilities
// ─────────────────────────────────────────────────────────────────────────

function isReservedIp(ip: string): boolean {
  if (ip.includes(':')) {
    const addr = ip.toLowerCase().replace(/^\[|\]$/g, '')
    if (addr === '::1')
      return true
    const firstWord = parseInt(addr.split(':')[0] ?? '0', 16)
    if ((firstWord & 0xffc0) === 0xfe80)
      return true // fe80::/10 link-local
    if ((firstWord & 0xfe00) === 0xfc00)
      return true // fc00::/7 unique-local
    if (addr.startsWith('::ffff:'))
      return isReservedIp(addr.slice(7))
    return false
  }

  const parts = ip.split('.').map(Number)
  if (parts.length !== 4 || parts.some((p) => Number.isNaN(p) || p < 0 || p > 255))
    return true

  const a = parts[0]!
  const b = parts[1] ?? 0
  const c = parts[2] ?? 0

  return (
    a === 0
    || a === 10
    || a === 127
    || (a === 100 && (b & 0xc0) === 64)
    || (a === 169 && b === 254)
    || (a === 172 && b >= 16 && b <= 31)
    || (a === 192 && b === 0 && (c === 0 || c === 2))
    || (a === 192 && b === 168)
    || (a === 198 && (b === 18 || b === 19))
    || (a === 198 && b === 51 && c === 100)
    || (a === 203 && b === 0 && c === 113)
    || a >= 224
  )
}

// ─────────────────────────────────────────────────────────────────────────
// DNS resolution
// ─────────────────────────────────────────────────────────────────────────

async function resolveAndCheckIp(hostname: string): Promise<string> {
  let addrs: Array<{ address: string, family: number }>
  try {
    addrs = await nodeDnsLookup(hostname, { all: true, family: 0 })
  } catch (err) {
    throw new Error(`fetch: DNS resolution failed for "${hostname}": ${(err as Error).message}`)
  }

  if (addrs.length === 0)
    throw new Error(`fetch: DNS resolution returned no addresses for "${hostname}"`)

  for (const { address } of addrs) {
    if (isReservedIp(address))
      throw new Error(`fetch: request blocked — "${hostname}" resolves to private/reserved IP ${address}`)
  }

  return addrs[0]!.address
}

// ─────────────────────────────────────────────────────────────────────────
// Hostname matching
// ─────────────────────────────────────────────────────────────────────────

function hostExact(requestHost: string, ruleHost: string): boolean {
  return !ruleHost.startsWith('*.') && requestHost === ruleHost
}

function hostWildcard(requestHost: string, ruleHost: string): boolean {
  if (!ruleHost.startsWith('*.'))
    return false
  const domain = ruleHost.slice(2)
  const expectedSegments = domain.split('.').length + 1
  return requestHost.split('.').length === expectedSegments && requestHost.endsWith(`.${domain}`)
}

// ─────────────────────────────────────────────────────────────────────────
// rou3 router — stores FetchRouteRule as data so we can retrieve it on match
// ─────────────────────────────────────────────────────────────────────────

interface CompiledRule<TCtx> {
  rule: FetchOriginRule<TCtx>
  hosts: string[]
  router: RouterContext<FetchRouteRule<TCtx>>
}

function routeMethodsToRou3(methods: FetchRouteRule['methods']): string[] {
  if (methods === undefined || methods === '*')
    return ['']
  if (typeof methods === 'string')
    return [methods.toUpperCase()]
  return methods.map((m) => m.toUpperCase())
}

function buildRouter<TCtx>(routes: FetchRouteRule<TCtx>[]): RouterContext<FetchRouteRule<TCtx>> {
  const router = createRouter<FetchRouteRule<TCtx>>()
  for (const route of routes) {
    for (const method of routeMethodsToRou3(route.methods)) {
      addRoute(router, method, route.path, route)
    }
  }
  return router
}

function compileRules<TCtx>(rawRules: FetchOriginRule<TCtx> | FetchOriginRule<TCtx>[]): CompiledRule<TCtx>[] {
  const rules = Array.isArray(rawRules) ? rawRules : [rawRules]
  return rules.map((rule) => ({
    rule,
    hosts: Array.isArray(rule.host) ? rule.host : [rule.host],
    router: buildRouter(rule.routes),
  }))
}

// ─────────────────────────────────────────────────────────────────────────
// Path normalization
// ─────────────────────────────────────────────────────────────────────────

function decodePath(rawPath: string): string {
  const once = decodeURIComponent(rawPath)
  const twice = decodeURIComponent(once)
  if (once !== twice)
    throw new Error('fetch: request denied — double-encoded path detected')
  return once
}

// ─────────────────────────────────────────────────────────────────────────
// Origin + route matching
// ─────────────────────────────────────────────────────────────────────────

type MatchResult<TCtx>
  = | { kind: 'allow', originRule: FetchOriginRule<TCtx>, matchedRule: FetchRouteRule<TCtx>, params: Record<string, string> }
    | { kind: 'deny-route' }
    | { kind: 'no-origin' }

/**
 * Two-pass origin matching: exact hostname rules are checked before wildcard
 * rules, regardless of array order. Within the same priority level the first
 * entry in the array wins. A `deny-route` result from any pass is final —
 * the explicit origin grant is exhaustive.
 * @param req
 * @param compiled
 */
function matchRules<TCtx>(req: SafeFetchRequest, compiled: CompiledRule<TCtx>[]): MatchResult<TCtx> {
  for (const pass of ['exact', 'wildcard'] as const) {
    for (const { rule, hosts, router } of compiled) {
      // Only consider rules that match at the current specificity level
      const hostOk = pass === 'exact'
        ? hosts.some((rh) => hostExact(req.host, rh))
        : hosts.some((rh) => hostWildcard(req.host, rh))
      if (!hostOk)
        continue

      if (rule.httpsOnly !== false && req.protocol !== 'https')
        continue

      const standardPort = req.protocol === 'https' ? 443 : 80
      if (rule.port !== undefined) {
        const allowed = Array.isArray(rule.port) ? rule.port : [rule.port]
        if (!allowed.includes(req.port))
          continue
      } else if (req.port !== standardPort) {
        continue
      }

      if (rule.routes.length === 0)
        return { kind: 'deny-route' }

      let decodedPathname: string
      try {
        decodedPathname = decodePath(new URL(req.url).pathname)
      } catch {
        return { kind: 'deny-route' }
      }

      const matched = findRoute(router, req.method, decodedPathname, { normalize: true })
      if (matched === undefined)
        return { kind: 'deny-route' }

      return { kind: 'allow', originRule: rule, matchedRule: matched.data, params: matched.params ?? {} }
    }
  }

  return { kind: 'no-origin' }
}

// ─────────────────────────────────────────────────────────────────────────
// FetchContext — mutable request-side context for middleware
// ─────────────────────────────────────────────────────────────────────────

function makeFetchContext<TCtx>(
  initialUrl: string,
  method: string,
  initialHeaders: Record<string, string>,
  initialBody: Uint8Array | string | null,
  params: Record<string, string>,
  hop: number,
  raw: HostFetchRequest,
  context: TCtx,
): FetchContext<TCtx> {
  let _url = initialUrl
  const _headers: Record<string, string> = { ...initialHeaders }

  const req: FetchContextReq = {
    method,
    params,
    hop,
    raw,
    body: initialBody,
    get url() {
      return _url
    },
    get headers() {
      return _headers
    },
    header(name: string, value: string): void {
      _headers[name.toLowerCase()] = value
    },
    setUrl(url: string): void {
      _url = url
    },
    setBody(b: Uint8Array | string | null): void {
      this.body = b
    },
  }

  return { req, res: null, context }
}

// ─────────────────────────────────────────────────────────────────────────
// HTTP client
// ─────────────────────────────────────────────────────────────────────────

function makeDnsLookupFn(): NonNullable<NonNullable<Parameters<typeof interceptors.dns>[0]>['lookup']> {
  return (hostnameOrUrl, _opts, callback) => {
    const hostname: string
      = typeof hostnameOrUrl === 'object' && hostnameOrUrl !== null
        ? (hostnameOrUrl as unknown as URL).hostname
        : hostnameOrUrl

    nodeDnsLookupCb(hostname, { all: true, family: 0 }, (err, addresses) => {
      if (err) {
        callback(err, [])
        return
      }
      if (addresses.length === 0) {
        callback(new Error(`fetch: no DNS addresses for "${hostname}"`), [])
        return
      }
      for (const { address } of addresses) {
        if (isReservedIp(address)) {
          callback(new Error(`fetch: request blocked — "${hostname}" resolves to private/reserved IP ${address}`), [])
          return
        }
      }
      callback(null, addresses.map((addr) => ({ address: addr.address, family: addr.family as 4 | 6, ttl: 10_000 })))
    })
  }
}

async function readBody(body: ReadableStream<Uint8Array> | null, maxBytes: number): Promise<Uint8Array | null> {
  if (body === null)
    return null
  const reader = body.getReader()
  const chunks: Uint8Array[] = []
  let total = 0
  try {
    for (;;) {
      const { done, value } = await reader.read()
      if (done)
        break
      total += value.byteLength
      if (total > maxBytes) {
        await reader.cancel()
        throw new Error(`fetch: response body exceeds maxBodyBytes limit (${maxBytes} bytes)`)
      }
      chunks.push(value)
    }
  } finally {
    reader.releaseLock()
  }
  const result = new Uint8Array(total)
  let offset = 0
  for (const chunk of chunks) {
    result.set(chunk, offset)
    offset += chunk.byteLength
  }
  return result
}

const REDIRECT_STATUSES = new Set([301, 302, 303, 307, 308])

async function httpRequest(
  url: string,
  method: string,
  headers: Record<string, string>,
  body: Uint8Array | string | null,
  agent: Dispatcher,
  timeoutMs: number,
  maxBodyBytes: number,
  incomingSignal?: AbortSignal,
): Promise<{ response: Awaited<ReturnType<typeof undiciFetch>>, bodyBytes: Uint8Array | null }> {
  const signals: AbortSignal[] = [AbortSignal.timeout(timeoutMs)]
  if (incomingSignal !== undefined)
    signals.push(incomingSignal)
  const signal = signals.length === 1 ? signals[0]! : AbortSignal.any(signals)

  const response = await undiciFetch(url, {
    method,
    headers,
    ...(body !== null ? { body } : {}),
    dispatcher: agent,
    signal,
    redirect: 'manual',
  })

  const bodyBytes = await readBody(response.body as ReadableStream<Uint8Array> | null, maxBodyBytes)
  return { response, bodyBytes }
}

function headersToRecord(headers: Headers): Record<string, string> {
  const out: Record<string, string> = {}
  headers.forEach((value, key) => {
    out[key] = value
  })
  return out
}

function toHostFetchResponse(
  response: Awaited<ReturnType<typeof undiciFetch>>,
  bodyBytes: Uint8Array | null,
): HostFetchResponse {
  return {
    status: response.status,
    statusText: response.statusText,
    headers: headersToRecord(response.headers),
    body: bodyBytes,
  }
}

// ─────────────────────────────────────────────────────────────────────────
// createSafeFetch
// ─────────────────────────────────────────────────────────────────────────

// ─────────────────────────────────────────────────────────────────────────
// Bridge arg validation
// ─────────────────────────────────────────────────────────────────────────

/**
 * Validates and parses the raw `...args: unknown[]` received from the bridge
 * when sandbox code calls the registered global/import function.
 *
 * Expected call shape (mirrors the web fetch API):
 *   fn(url: string, init?: { method?: string, headers?: Record<string,string>, body?: string | Uint8Array | null })
 *
 * Throws a descriptive Error (visible to sandbox code) on any type mismatch.
 * @param args
 */
function parseFetchArgs(args: unknown[]): HostFetchRequest {
  const url = args[0]
  if (typeof url !== 'string' || url.length === 0)
    throw new Error('fetch: first argument must be a non-empty URL string')

  const init = args[1]
  if (init !== undefined && init !== null) {
    if (typeof init !== 'object' || Array.isArray(init))
      throw new Error('fetch: second argument must be a plain object')
  }

  const initObj = (init ?? {}) as Record<string, unknown>

  const method = initObj['method']
  if (method !== undefined && typeof method !== 'string')
    throw new Error('fetch: init.method must be a string')

  const rawHeaders = initObj['headers']
  const headers: Record<string, string> = {}
  if (rawHeaders !== undefined && rawHeaders !== null) {
    if (typeof rawHeaders !== 'object' || Array.isArray(rawHeaders))
      throw new Error('fetch: init.headers must be a plain object')
    for (const [k, v] of Object.entries(rawHeaders as Record<string, unknown>)) {
      if (typeof v !== 'string')
        throw new Error(`fetch: init.headers["${k}"] must be a string`)
      // Reject CRLF/NUL in names or values — prevents HTTP request splitting
      // eslint-disable-next-line no-control-regex
      if (/[\r\n\x00]/.test(k) || /[\r\n\x00]/.test(v))
        throw new Error(`fetch: header "${k}" contains illegal characters (CR, LF, or NUL)`)
      // Strip host — the transport layer derives it from the URL; agent cannot override it
      if (k.toLowerCase() === 'host')
        continue
      headers[k.toLowerCase()] = v
    }
  }

  const body = initObj['body'] ?? null
  if (body !== null && typeof body !== 'string' && !(body instanceof Uint8Array))
    throw new Error('fetch: init.body must be a string, Uint8Array, or null')

  return {
    url,
    method: typeof method === 'string' ? method.toUpperCase() : 'GET',
    headers,
    body: body as string | Uint8Array | null,
  }
}

/**
 * Build a `BridgeWithShim` global that the sandbox layer wires into the
 * prefix as both a secure bridge handler and a sandbox-side response wrapper.
 *
 * Pass directly as a global value:
 * ```ts
 * await sandbox.precompile({
 *   code: prefixSource,
 *   globals: { fetch: createSafeFetch({ rules: [...] }) },
 * })
 * ```
 *
 * Inside the sandbox the agent calls `fetch(url, init)` and gets back a
 * response object with `.ok`, `.json()`, `.text()`, and `.bytes()`.
 * @param options
 */
export function createSafeFetch<TCtx = undefined>(options: SafeFetchOptions<TCtx>): SafeFetchGlobal<TCtx> {
  const { handler, invoke } = buildSafeFetchHandler(options)
  return {
    kind: 'bridge-with-shim',
    handler,
    invoke,
    shim: `(result) => ({
      ...result,
      ok:    result.status >= 200 && result.status < 300,
      json:  () => JSON.parse(new TextDecoder().decode(result.body)),
      text:  () => new TextDecoder().decode(result.body),
      bytes: () => result.body instanceof Uint8Array
                 ? result.body
                 : new TextEncoder().encode(String(result.body ?? '')),
    })`,
  }
}

/**
 * Internal: builds the host-side entry points without the shim.
 *
 * Returns both:
 * - `handler` — the bridge `SafeFetchFn` (`(...args)` from the sandbox; no
 *   host context, so `ctx.context` is `undefined`).
 * - `invoke` — the typed host-side entry that threads a per-call `context`
 *   onto the `FetchContext`.
 *
 * Both share the same compiled rules, DNS agent, and middleware pipeline —
 * only the per-call `HostFetchRequest` + `context` differ, so concurrent
 * invocations never observe each other's context.
 * @param options
 */
function buildSafeFetchHandler<TCtx>(
  options: SafeFetchOptions<TCtx>,
): { handler: SafeFetchFn, invoke: SafeFetchInvoke<TCtx> } {
  if (options.rules === undefined && options.policy === undefined)
    throw new TypeError('createSafeFetch: at least one of `rules` or `policy` must be provided')

  const compiled: CompiledRule<TCtx>[] = options.rules !== undefined ? compileRules(options.rules) : []
  const doPinDns = options.pinDns !== false
  const maxRedirects = options.maxRedirects ?? 0
  const timeoutMs = options.timeoutMs ?? 30_000
  const maxBodyBytes = options.maxBodyBytes ?? 16 * 1024 * 1024
  const allowCompressed = options.allowCompressedResponses ?? false

  const agent: Dispatcher = doPinDns
    ? new Agent().compose(interceptors.dns({ lookup: makeDnsLookupFn(), maxTTL: 10_000, dualStack: false, affinity: 4 }))
    : new Agent()

  async function buildSafeFetchRequest(parsedUrl: URL, method: string, headers: Record<string, string>, hop: number): Promise<SafeFetchRequest> {
    const protocol = parsedUrl.protocol.slice(0, -1)
    if (protocol !== 'http' && protocol !== 'https')
      throw new Error(`fetch: unsupported protocol "${parsedUrl.protocol}"`)

    const host = parsedUrl.hostname.toLowerCase()
    const port = parsedUrl.port !== '' ? parseInt(parsedUrl.port, 10) : protocol === 'https' ? 443 : 80

    let resolvedIp: string | null = null
    if (doPinDns) {
      const isIpLiteral = /^\d{1,3}(?:\.\d{1,3}){3}$/.test(host) || host.startsWith('[')
      if (!isIpLiteral) {
        resolvedIp = await resolveAndCheckIp(host)
      } else if (isReservedIp(host.replace(/^\[|\]$/g, ''))) {
        throw new Error(`fetch: request blocked — IP literal ${host} is private/reserved`)
      }
    }

    return { url: parsedUrl.toString(), protocol: protocol as 'http' | 'https', host, port, path: parsedUrl.pathname + parsedUrl.search, method, headers, resolvedIp, hop }
  }

  async function runDeny(req: SafeFetchRequest, reason: string): Promise<never> {
    try {
      const _p = options.onDenied?.(req, reason)
      _p?.catch(() => undefined)
    } catch { /* silently ignore */ }
    throw new Error(reason)
  }

  async function checkRequest(
    req: SafeFetchRequest,
  ): Promise<{ originRule: FetchOriginRule<TCtx>, matchedRule: FetchRouteRule<TCtx>, params: Record<string, string> } | null> {
    if (compiled.length > 0) {
      const result = matchRules(req, compiled)
      if (result.kind === 'allow')
        return { originRule: result.originRule, matchedRule: result.matchedRule, params: result.params }
      if (result.kind === 'deny-route')
        return runDeny(req, `fetch: request denied — no matching route for ${req.method} ${req.path} on ${req.host}`)
    }

    if (options.policy !== undefined) {
      let allowed: boolean
      try {
        allowed = await options.policy(req)
      } catch (err) {
        const reason = (err as Error).message || 'request denied by policy'
        try {
          const _p = options.onDenied?.(req, reason)
          _p?.catch(() => undefined)
        } catch { /* ignore */ }
        throw err instanceof Error ? err : new Error(reason)
      }
      if (allowed)
        return null
    }

    return runDeny(req, 'fetch: request denied by policy')
  }

  /**
   * Compose the middleware chain.
   *
   * Each middleware receives `next` = a function that runs the rest of the
   * chain and sets `ctx.res`. If a middleware returns a `HostFetchResponse`
   * the chain sets `ctx.res` to that value immediately (outer middleware still
   * runs its after-next code). If middleware returns void the current `ctx.res`
   * propagates. If middleware throws the whole chain rejects.
   *
   * `finalNext` is the innermost handler: real HTTP call + redirect loop.
   * It sets `ctx.res` and returns void.
   * @param middlewares
   * @param ctx
   * @param finalNext
   */
  function runMiddlewareChain(
    middlewares: Array<FetchMiddleware<TCtx> | undefined>,
    ctx: FetchContext<TCtx>,
    finalNext: () => Promise<void>,
  ): Promise<HostFetchResponse> {
    const active = middlewares.filter((m): m is FetchMiddleware<TCtx> => m !== undefined)

    const dispatch = async (i: number): Promise<void> => {
      if (i >= active.length) {
        await finalNext() // sets ctx.res
        return
      }
      const result = await active[i]!(ctx, () => dispatch(i + 1))
      // If middleware returned a response, install it on ctx.res.
      // void / undefined means "keep whatever downstream set".
      if (result !== undefined && result !== null) {
        ctx.res = result
      }
    }

    return dispatch(0).then(() => {
      if (ctx.res === null)
        throw new Error('fetch: middleware chain produced no response — at least one middleware must call next() or return a response')
      return ctx.res
    })
  }

  // Core pipeline shared by both entry points. `context` is threaded onto the
  // FetchContext so middleware can read it; it is per-call, never shared state.
  const core = async (hostRequest: HostFetchRequest, context: TCtx): Promise<HostFetchResponse> => {
    let parsedUrl: URL
    try {
      parsedUrl = new URL(hostRequest.url)
    } catch {
      throw new Error(`fetch: invalid URL "${hostRequest.url}"`)
    }

    // Strip userinfo — agent cannot inject HTTP Basic Auth credentials via the URL
    if (parsedUrl.username !== '' || parsedUrl.password !== '') {
      parsedUrl.username = ''
      parsedUrl.password = ''
    }

    const baseHeaders: Record<string, string> = { ...hostRequest.headers }
    if (!allowCompressed)
      baseHeaders['accept-encoding'] = 'identity'
    // host is always derived from the URL by the transport layer; removing any
    // agent-supplied value here ensures it cannot be injected at the HTTP layer
    delete baseHeaders['host']

    const method = hostRequest.method.toUpperCase()

    // Allow/deny check on the initial URL
    const req = await buildSafeFetchRequest(parsedUrl, method, baseHeaders, 0)
    const match = await checkRequest(req)

    // Build mutable context — middleware sees and mutates this
    const ctx = makeFetchContext<TCtx>(
      parsedUrl.toString(),
      method,
      baseHeaders,
      hostRequest.body,
      match?.params ?? {},
      0,
      hostRequest,
      context,
    )

    // Collect middleware layers: global → origin → route
    const middlewares: Array<FetchMiddleware<TCtx> | undefined> = [
      options.middleware,
      match?.originRule.middleware,
      match?.matchedRule.middleware,
    ]

    // The innermost next: real HTTP call + redirect following.
    // Sets ctx.res; returns void so the chain propagates via ctx.res.
    const finalNext = async (): Promise<void> => {
      let currentUrl = ctx.req.url
      let currentMethod = method
      let currentBody = ctx.req.body
      let hop = 0

      for (;;) {
        const { response, bodyBytes } = await httpRequest(
          currentUrl,
          currentMethod,
          ctx.req.headers,
          currentBody,
          agent,
          timeoutMs,
          maxBodyBytes,
          hostRequest.signal,
        )

        if (!REDIRECT_STATUSES.has(response.status) || hop >= maxRedirects) {
          ctx.res = toHostFetchResponse(response, bodyBytes)
          return
        }

        const location = response.headers.get('location')
        if (location === null) {
          ctx.res = toHostFetchResponse(response, bodyBytes)
          return
        }

        let nextUrl: URL
        try {
          nextUrl = new URL(location, currentUrl)
        } catch {
          ctx.res = toHostFetchResponse(response, bodyBytes)
          return
        }

        // Security check for the redirect destination
        hop++
        const redirectReq = await buildSafeFetchRequest(nextUrl, currentMethod, ctx.req.headers, hop)
        await checkRequest(redirectReq)

        if (response.status === 303 || ((response.status === 301 || response.status === 302) && currentMethod === 'POST')) {
          currentMethod = 'GET'
          currentBody = null
        }
        currentUrl = nextUrl.toString()
      }
    }

    return runMiddlewareChain(middlewares, ctx, finalNext)
  }

  // Bridge path: the sandbox supplies `(url, init)`; there is no host caller,
  // so `context` is `undefined`. `async` so a `parseFetchArgs` throw surfaces
  // as a rejected promise rather than a synchronous throw.
  const handler: SafeFetchFn = async (...args: unknown[]): Promise<HostFetchResponse> =>
    core(parseFetchArgs(args), undefined as unknown as TCtx)

  // Host-side path: a structured request plus the typed per-call context.
  const invoke: SafeFetchInvoke<TCtx> = async (request, context): Promise<HostFetchResponse> => {
    const hostRequest = parseFetchArgs([
      request.url,
      { method: request.method, headers: request.headers, body: request.body },
    ])
    if (request.signal !== undefined)
      hostRequest.signal = request.signal
    return core(hostRequest, context)
  }

  return { handler, invoke }
}
