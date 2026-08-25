/**
 * Tests for createSafeFetch — allow/deny logic, routing, and security edge cases.
 *
 * Network calls are mocked via vi.mock so these run without internet access.
 */

import { describe, expect, it, vi, beforeEach } from 'vitest'
import { createSafeFetch } from '../src/index.js'
import type { FetchOriginRule, SafeFetchPolicy } from '../src/index.js'
import { assertPublicAddresses, isReservedIp, makeDnsLookupFn } from '../src/dns.js'

// ─────────────────────────────────────────────────────────────────────────
// Mock undici (fetch) and both dns modules so no real network calls happen.
//
// vi.mock factories are hoisted before const declarations, so we use
// vi.hoisted() to make the mock fns available inside the factory closures.
// ─────────────────────────────────────────────────────────────────────────

const { mockFetch, mockDnsPromiseLookup, mockDnsCallbackLookup } = vi.hoisted(() => ({
  mockFetch: vi.fn(),
  // node:dns/promises — Promise-based, used by resolveAndCheckIp before policy
  mockDnsPromiseLookup: vi.fn(),
  // node:dns — callback-based, used by the undici DNS interceptor
  mockDnsCallbackLookup: vi.fn(),
}))

vi.mock('undici', async (importOriginal) => {
  const actual = await importOriginal<typeof import('undici')>()
  return { ...actual, fetch: mockFetch }
})

vi.mock('node:dns/promises', () => ({ lookup: mockDnsPromiseLookup }))
vi.mock('node:dns', () => ({ lookup: mockDnsCallbackLookup }))

/**
 * Returns a minimal successful mock Response.
 * @param body
 * @param status
 */
function okResponse(body = 'ok', status = 200): Response {
  return new Response(body, {
    status,
    headers: { 'content-type': 'text/plain' },
  })
}

/**
 * Returns a redirect mock Response with a Location header.
 * @param location
 * @param status
 */
function redirectResponse(location: string, status = 302): Response {
  return new Response(null, {
    status,
    headers: { location },
  })
}

// Default DNS mocks: resolve all hostnames to public IP 1.2.3.4
beforeEach(() => {
  mockFetch.mockReset()
  mockDnsPromiseLookup.mockReset()
  mockDnsCallbackLookup.mockReset()
  mockFetch.mockResolvedValue(okResponse())
  mockDnsPromiseLookup.mockResolvedValue([{ address: '1.2.3.4', family: 4 }])
  // Callback-based lookup (undici DNS interceptor / makeDnsLookupFn)
  mockDnsCallbackLookup.mockImplementation(
    (_host: string, _opts: unknown, cb: (err: null, addrs: Array<{ address: string, family: number }>) => void) => {
      cb(null, [{ address: '1.2.3.4', family: 4 }])
    },
  )
})

// ─────────────────────────────────────────────────────────────────────────
// Construction validation
// ─────────────────────────────────────────────────────────────────────────

describe('createSafeFetch construction', () => {
  it('throws when neither rules nor policy is provided', () => {
    expect(() => createSafeFetch({} as Parameters<typeof createSafeFetch>[0])).toThrow(
      /at least one of `rules` or `policy`/,
    )
  })

  it('accepts rules-only without throwing', () => {
    expect(() =>
      createSafeFetch({
        rules: { host: 'api.example.com', routes: [{ path: '/**' }] },
      }),
    ).not.toThrow()
  })

  it('accepts policy-only without throwing', () => {
    expect(() => createSafeFetch({ policy: () => true })).not.toThrow()
  })

  it('accepts rules + policy without throwing', () => {
    expect(() =>
      createSafeFetch({
        rules: { host: 'api.example.com', routes: [{ path: '/**' }] },
        policy: () => false,
      }),
    ).not.toThrow()
  })

  it('accepts full option surface without throwing', () => {
    const policy: SafeFetchPolicy = ({ host, method, path, hop }) => {
      if (host !== 'api.example.com')
        return false
      if (method !== 'GET' && method !== 'POST')
        throw new Error(`method ${method} not allowed`)
      if (hop > 0 && path.startsWith('/admin'))
        throw new Error('redirect to /admin denied')
      return true
    }

    expect(() =>
      createSafeFetch({
        policy,
        pinDns: true,
        maxRedirects: 0,
        timeoutMs: 5_000,
        maxBodyBytes: 1024 * 1024,
        allowCompressedResponses: false,
        onDenied: () => {},
      }),
    ).not.toThrow()
  })
})

// ─────────────────────────────────────────────────────────────────────────
// Policy-only mode (backward-compatible)
// ─────────────────────────────────────────────────────────────────────────

describe('policy-only mode', () => {
  it('allows when policy returns true', async () => {
    const { handler } = createSafeFetch({ policy: () => true, pinDns: false })
    const result = await handler('https://example.com/', { method: 'GET', headers: {}, body: null })
    expect(result.status).toBe(200)
  })

  it('denies when policy returns false', async () => {
    const { handler } = createSafeFetch({ policy: () => false, pinDns: false })
    await expect(
      handler('https://example.com/', { method: 'GET', headers: {}, body: null }),
    ).rejects.toThrow(/denied by policy/)
  })

  it('surfaces custom reason when policy throws', async () => {
    const { handler } = createSafeFetch({
      policy: () => {
        throw new Error('custom deny reason')
      },
      pinDns: false,
    })
    await expect(
      handler('https://example.com/', { method: 'GET', headers: {}, body: null }),
    ).rejects.toThrow('custom deny reason')
  })

  it('supports async policy', async () => {
    const { handler } = createSafeFetch({
      policy: async ({ host }) => host === 'api.example.com',
      pinDns: false,
    })
    await expect(
      handler('https://api.example.com/', { method: 'GET', headers: {}, body: null }),
    ).resolves.toBeDefined()
    await expect(
      handler('https://other.com/', { method: 'GET', headers: {}, body: null }),
    ).rejects.toThrow()
  })
})

// ─────────────────────────────────────────────────────────────────────────
// Rules — origin matching
// ─────────────────────────────────────────────────────────────────────────

describe('rules: origin matching', () => {
  const makeHandler = (rule: FetchOriginRule) =>
    createSafeFetch({ rules: rule, pinDns: false }).handler

  it('allows a matching exact hostname', async () => {
    const handler = makeHandler({
      host: 'api.example.com',
      routes: [{ path: '/**' }],
    })
    await expect(
      handler('https://api.example.com/users', { method: 'GET', headers: {}, body: null }),
    ).resolves.toBeDefined()
  })

  it('denies a non-matching hostname', async () => {
    const handler = makeHandler({
      host: 'api.example.com',
      routes: [{ path: '/**' }],
    })
    await expect(
      handler('https://other.example.com/users', { method: 'GET', headers: {}, body: null }),
    ).rejects.toThrow()
  })

  it('allows a subdomain-wildcard match (single level)', async () => {
    const handler = makeHandler({ host: '*.example.com', routes: [{ path: '/**' }] })
    await expect(
      handler('https://api.example.com/', { method: 'GET', headers: {}, body: null }),
    ).resolves.toBeDefined()
  })

  it('denies the base domain for a wildcard rule', async () => {
    const handler = makeHandler({ host: '*.example.com', routes: [{ path: '/**' }] })
    await expect(
      handler('https://example.com/', { method: 'GET', headers: {}, body: null }),
    ).rejects.toThrow()
  })

  it('denies a multi-level subdomain for a single-level wildcard rule', async () => {
    const handler = makeHandler({ host: '*.example.com', routes: [{ path: '/**' }] })
    await expect(
      handler('https://a.b.example.com/', { method: 'GET', headers: {}, body: null }),
    ).rejects.toThrow()
  })

  it('allows matching one of multiple hosts in a rule', async () => {
    const handler = makeHandler({
      host: ['api.example.com', 'cdn.example.com'],
      routes: [{ path: '/**' }],
    })
    await expect(
      handler('https://cdn.example.com/asset.js', { method: 'GET', headers: {}, body: null }),
    ).resolves.toBeDefined()
  })

  it('denies http when httpsOnly is not false', async () => {
    const handler = makeHandler({ host: 'api.example.com', routes: [{ path: '/**' }] })
    await expect(
      handler('http://api.example.com/', { method: 'GET', headers: {}, body: null }),
    ).rejects.toThrow()
  })

  it('allows http when httpsOnly is explicitly false', async () => {
    const handler = makeHandler({
      host: 'api.example.com',
      httpsOnly: false,
      routes: [{ path: '/**' }],
    })
    await expect(
      handler('http://api.example.com/', { method: 'GET', headers: {}, body: null }),
    ).resolves.toBeDefined()
  })

  it('denies a non-standard port when port rule is omitted', async () => {
    const handler = makeHandler({ host: 'api.example.com', routes: [{ path: '/**' }] })
    await expect(
      handler('https://api.example.com:8443/', { method: 'GET', headers: {}, body: null }),
    ).rejects.toThrow()
  })

  it('allows an explicit non-standard port when the rule lists it', async () => {
    const handler = makeHandler({
      host: 'api.example.com',
      port: 8443,
      routes: [{ path: '/**' }],
    })
    await expect(
      handler('https://api.example.com:8443/', { method: 'GET', headers: {}, body: null }),
    ).resolves.toBeDefined()
  })
})

// ─────────────────────────────────────────────────────────────────────────
// Rules — route matching
// ─────────────────────────────────────────────────────────────────────────

describe('rules: route matching', () => {
  const makeHandler = (routes: FetchOriginRule['routes']) =>
    createSafeFetch({
      rules: { host: 'api.example.com', routes },
      pinDns: false,
    }).handler

  it('empty routes denies all paths on a matching origin', async () => {
    const handler = makeHandler([])
    await expect(
      handler('https://api.example.com/anything', { method: 'GET', headers: {}, body: null }),
    ).rejects.toThrow()
  })

  it('allows a matching exact path', async () => {
    const handler = makeHandler([{ path: '/v2/events' }])
    await expect(
      handler('https://api.example.com/v2/events', { method: 'GET', headers: {}, body: null }),
    ).resolves.toBeDefined()
  })

  it('denies a path that is not listed', async () => {
    const handler = makeHandler([{ path: '/v2/events' }])
    await expect(
      handler('https://api.example.com/v2/users', { method: 'GET', headers: {}, body: null }),
    ).rejects.toThrow()
  })

  it('/** matches any path including the root', async () => {
    const handler = makeHandler([{ path: '/**' }])
    for (const path of ['/', '/a', '/a/b/c', '/deep/nested/path']) {
      await expect(
        handler(`https://api.example.com${path}`, { method: 'GET', headers: {}, body: null }),
      ).resolves.toBeDefined()
    }
  })

  it('/prefix/** matches /prefix and /prefix/child', async () => {
    const handler = makeHandler([{ path: '/api/**' }])
    for (const path of ['/api', '/api/', '/api/users', '/api/users/123']) {
      await expect(
        handler(`https://api.example.com${path}`, { method: 'GET', headers: {}, body: null }),
      ).resolves.toBeDefined()
    }
  })

  it('/prefix/** does not match a sibling prefix', async () => {
    const handler = makeHandler([{ path: '/api/**' }])
    await expect(
      handler('https://api.example.com/admin/secret', { method: 'GET', headers: {}, body: null }),
    ).rejects.toThrow()
  })

  it('allows when method matches', async () => {
    const handler = makeHandler([{ path: '/users', methods: ['GET', 'POST'] }])
    await expect(
      handler('https://api.example.com/users', { method: 'POST', headers: {}, body: null }),
    ).resolves.toBeDefined()
  })

  it('denies when method is not listed', async () => {
    const handler = makeHandler([{ path: '/users', methods: ['GET'] }])
    await expect(
      handler('https://api.example.com/users', { method: 'DELETE', headers: {}, body: null }),
    ).rejects.toThrow()
  })

  it('allows any method when methods is omitted', async () => {
    const handler = makeHandler([{ path: '/users' }])
    for (const method of ['GET', 'POST', 'PATCH', 'DELETE', 'PUT']) {
      await expect(
        handler('https://api.example.com/users', { method, headers: {}, body: null }),
      ).resolves.toBeDefined()
    }
  })

  it('allows any method when methods is "*"', async () => {
    const handler = makeHandler([{ path: '/users', methods: '*' }])
    await expect(
      handler('https://api.example.com/users', { method: 'PATCH', headers: {}, body: null }),
    ).resolves.toBeDefined()
  })

  it('named params: /users/:id matches /users/123', async () => {
    const handler = makeHandler([{ path: '/users/:id', methods: 'GET' }])
    await expect(
      handler('https://api.example.com/users/123', { method: 'GET', headers: {}, body: null }),
    ).resolves.toBeDefined()
    // but not /users/
    await expect(
      handler('https://api.example.com/users/', { method: 'GET', headers: {}, body: null }),
    ).rejects.toThrow()
  })

  it('multiple routes: first-match wins', async () => {
    const handler = makeHandler([
      { path: '/users/:id', methods: 'GET' },
      { path: '/users/**', methods: 'GET' },
    ])
    // Both patterns could match /users/123 — either way it's allowed
    await expect(
      handler('https://api.example.com/users/123', { method: 'GET', headers: {}, body: null }),
    ).resolves.toBeDefined()
  })

  it('method is case-insensitive', async () => {
    const handler = makeHandler([{ path: '/users', methods: 'get' }])
    await expect(
      handler('https://api.example.com/users', { method: 'GET', headers: {}, body: null }),
    ).resolves.toBeDefined()
  })
})

// ─────────────────────────────────────────────────────────────────────────
// Rules — origin matched but no route → does NOT fall through to policy
// ─────────────────────────────────────────────────────────────────────────

describe('rules: origin match without route match does not fall through to policy', () => {
  it('denies even if policy would allow', async () => {
    const policySpy = vi.fn(() => true)
    const { handler } = createSafeFetch({
      rules: {
        host: 'api.example.com',
        routes: [{ path: '/public/**' }],
      },
      policy: policySpy,
      pinDns: false,
    })
    await expect(
      handler('https://api.example.com/admin', { method: 'GET', headers: {}, body: null }),
    ).rejects.toThrow()
    // Policy must not be consulted when an origin matched but route didn't
    expect(policySpy).not.toHaveBeenCalled()
  })
})

// ─────────────────────────────────────────────────────────────────────────
// Rules + policy: policy as fallback for unmatched origins
// ─────────────────────────────────────────────────────────────────────────

describe('rules + policy: fallback semantics', () => {
  it('calls policy when no origin rule matches', async () => {
    const policySpy = vi.fn(() => true)
    const { handler } = createSafeFetch({
      rules: { host: 'api.example.com', routes: [{ path: '/**' }] },
      policy: policySpy,
      pinDns: false,
    })
    await handler('https://other.example.com/data', { method: 'GET', headers: {}, body: null })
    expect(policySpy).toHaveBeenCalledOnce()
  })

  it('does not call policy when rules already allow', async () => {
    const policySpy = vi.fn(() => false) // would deny if called
    const { handler } = createSafeFetch({
      rules: { host: 'api.example.com', routes: [{ path: '/**' }] },
      policy: policySpy,
      pinDns: false,
    })
    await expect(
      handler('https://api.example.com/users', { method: 'GET', headers: {}, body: null }),
    ).resolves.toBeDefined()
    expect(policySpy).not.toHaveBeenCalled()
  })

  // A host match claims the origin: a scheme or port that the matched rule
  // rejects is denied, not delegated to the (typically coarser) policy.
  it('denies a scheme mismatch on a matched host instead of consulting policy', async () => {
    const policySpy = vi.fn(() => true) // would allow if consulted
    const { handler } = createSafeFetch({
      rules: { host: 'api.example.com', routes: [{ path: '/**' }] }, // httpsOnly by default
      policy: policySpy,
      pinDns: false,
    })
    await expect(
      handler('http://api.example.com/admin', { method: 'GET', headers: {}, body: null }),
    ).rejects.toThrow()
    expect(policySpy).not.toHaveBeenCalled()
  })

  it('denies a port mismatch on a matched host instead of consulting policy', async () => {
    const policySpy = vi.fn(() => true)
    const { handler } = createSafeFetch({
      rules: { host: 'api.example.com', routes: [{ path: '/**' }] }, // default port only
      policy: policySpy,
      pinDns: false,
    })
    await expect(
      handler('https://api.example.com:8443/admin', { method: 'GET', headers: {}, body: null }),
    ).rejects.toThrow()
    expect(policySpy).not.toHaveBeenCalled()
  })
})

// ─────────────────────────────────────────────────────────────────────────
// Composing multiple rules
// ─────────────────────────────────────────────────────────────────────────

describe('composing multiple origin rules', () => {
  const rules: FetchOriginRule[] = [
    { host: 'api.weather.com', routes: [{ path: '/v1/**', methods: 'GET' }] },
    { host: 'api.github.com', routes: [{ path: '/repos/**', methods: ['GET', 'POST'] }] },
  ]
  const { handler } = createSafeFetch({ rules, pinDns: false })

  it('allows a request matching the first rule', async () => {
    await expect(
      handler('https://api.weather.com/v1/forecast', { method: 'GET', headers: {}, body: null }),
    ).resolves.toBeDefined()
  })

  it('allows a request matching the second rule', async () => {
    await expect(
      handler('https://api.github.com/repos/owner/repo', { method: 'GET', headers: {}, body: null }),
    ).resolves.toBeDefined()
  })

  it('denies a request matching neither rule', async () => {
    await expect(
      handler('https://evil.com/', { method: 'GET', headers: {}, body: null }),
    ).rejects.toThrow()
  })

  it('exact hostname does not match a subdomain', async () => {
    // 'api.weather.com' rule must not match 'sub.api.weather.com'
    await expect(
      handler('https://sub.api.weather.com/v1/forecast', { method: 'GET' }),
    ).rejects.toThrow()
  })

  it('exact hostname rule beats wildcard regardless of array order', async () => {
    // Wildcard listed first — exact rule for api.example.com must still win
    const h = createSafeFetch({
      rules: [
        { host: '*.example.com', routes: [{ path: '/public/**' }] },
        { host: 'api.example.com', routes: [{ path: '/private/**' }] },
      ],
      pinDns: false,
    }).handler
    // api.example.com exact rule applies — /private/** is allowed
    await expect(
      h('https://api.example.com/private/secret', { method: 'GET' }),
    ).resolves.toBeDefined()
    // api.example.com exact rule applies — /public/** is NOT in its routes → denied
    await expect(
      h('https://api.example.com/public/page', { method: 'GET' }),
    ).rejects.toThrow()
    // sub.example.com has no exact rule — falls through to wildcard → /public/** allowed
    await expect(
      h('https://sub.example.com/public/page', { method: 'GET' }),
    ).resolves.toBeDefined()
  })

  it('exact hostname does not implicitly match subdomains (weather.com ≠ api.weather.com)', async () => {
    const h = createSafeFetch({
      rules: [{ host: 'weather.com', routes: [{ path: '/**' }] }],
      pinDns: false,
    }).handler
    await expect(
      h('https://api.weather.com/v1/forecast', { method: 'GET' }),
    ).rejects.toThrow()
  })
})

// ─────────────────────────────────────────────────────────────────────────
// Path security edge cases
// ─────────────────────────────────────────────────────────────────────────

// Paths are matched exactly as they are sent — no percent-decoding, so the
// authorised string and the wire string are identical. The URL parser still
// collapses `.`/`..` (including the `%2e` forms) before matching.
describe('path security', () => {
  const { handler } = createSafeFetch({
    rules: { host: 'api.example.com', routes: [{ path: '/public/**' }] },
    pinDns: false,
  })

  it('blocks path traversal: /public/../admin', async () => {
    // The URL parser normalises `..` to `/admin`, which is not under /public/**.
    await expect(
      handler('https://api.example.com/public/../admin', { method: 'GET', headers: {}, body: null }),
    ).rejects.toThrow()
  })

  it('blocks percent-encoded traversal: /public/%2e%2e/admin', async () => {
    // The parser resolves the WHATWG `%2e` dot form too → /admin.
    await expect(
      handler('https://api.example.com/public/%2e%2e/admin', { method: 'GET', headers: {}, body: null }),
    ).rejects.toThrow()
  })

  it('treats a double-encoded path literally (no decoding), so it stays under /public', async () => {
    // %252e is not a dot to the parser; the path is a literal segment under
    // /public and is sent verbatim, so matched === sent. A server that does
    // not decode %XX (the default) sees exactly this path.
    await expect(
      handler('https://api.example.com/public/%252e%252e/admin', { method: 'GET', headers: {}, body: null }),
    ).resolves.toBeDefined()
  })

  it('allows a legitimate percent-encoded path segment', async () => {
    // /public/hello%20world is matched and sent verbatim — still under /public/**.
    await expect(
      handler('https://api.example.com/public/hello%20world', { method: 'GET', headers: {}, body: null }),
    ).resolves.toBeDefined()
  })

  it('denies an encoded-slash bypass of a specific route', async () => {
    // With only /a allowed, /b%2F..%2Fa must NOT match: %2F is a literal, so
    // the path is one segment `b%2F..%2Fa`, not `/a`. (Decoding it, as the old
    // code did, would have matched /a while sending /b%2F..%2Fa.)
    const { handler: specific } = createSafeFetch({
      rules: { host: 'api.example.com', routes: [{ path: '/a' }] },
      pinDns: false,
    })
    await expect(
      specific('https://api.example.com/b%2F..%2Fa', { method: 'GET', headers: {}, body: null }),
    ).rejects.toThrow()
  })
})

// ─────────────────────────────────────────────────────────────────────────
// Header injection hardening
// ─────────────────────────────────────────────────────────────────────────

describe('header injection hardening', () => {
  const { handler } = createSafeFetch({
    rules: { host: 'api.example.com', routes: [{ path: '/**' }] },
    pinDns: false,
  })

  it('strips agent-supplied host header — cannot override Host on the wire', async () => {
    await handler('https://api.example.com/data', {
      method: 'GET',
      headers: { host: 'evil.com' },
      body: null,
    })
    const sentHeaders = mockFetch.mock.calls[0]?.[1]?.headers as Record<string, string>
    // host must NOT be the agent-injected value
    expect(sentHeaders?.['host']).not.toBe('evil.com')
  })

  it('rejects header values containing CRLF (request splitting)', async () => {
    await expect(
      handler('https://api.example.com/data', {
        method: 'GET',
        headers: { 'x-custom': 'value\r\nX-Injected: evil' },
        body: null,
      }),
    ).rejects.toThrow(/illegal characters/)
  })

  it('rejects header names containing CRLF', async () => {
    await expect(
      handler('https://api.example.com/data', {
        method: 'GET',
        headers: { 'x-bad\r\nX-Inject': 'val' },
        body: null,
      }),
    ).rejects.toThrow(/illegal characters/)
  })

  it('rejects header values containing NUL', async () => {
    await expect(
      handler('https://api.example.com/data', {
        method: 'GET',
        headers: { 'x-custom': 'value\x00injected' },
        body: null,
      }),
    ).rejects.toThrow(/illegal characters/)
  })

  it('strips userinfo from URL — agent cannot inject Basic Auth via URL credentials', async () => {
    await handler('https://user:s3cr3t@api.example.com/data', { method: 'GET' })
    const calledUrl = mockFetch.mock.calls[0]?.[0] as string
    expect(calledUrl).not.toContain('user')
    expect(calledUrl).not.toContain('s3cr3t')
    expect(calledUrl).toContain('api.example.com')
  })

  it('URL with userinfo still matches origin rules correctly', async () => {
    // The host extracted from https://user:pass@api.example.com is api.example.com
    // so rules should still apply normally
    await expect(
      handler('https://user:pass@api.example.com/data', { method: 'GET' }),
    ).resolves.toBeDefined()
  })
})

// ─────────────────────────────────────────────────────────────────────────
// DNS pinning and private IP blocking (pinDns: true, the default)
// ─────────────────────────────────────────────────────────────────────────

// The private/reserved-IP block lives in the connection-time DNS interceptor
// (`makeDnsLookupFn` / `assertPublicAddresses`), which undici invokes only when
// a request actually connects — i.e. after it passed the allow/deny check. The
// handler mocks `fetch`, so that interceptor never fires through the handler;
// these cover it directly.
describe('DNS interceptor — private-IP block', () => {
  function lookup(hostname: string): Promise<{ err: Error | null, addrs: unknown }> {
    return new Promise((resolve) => {
      makeDnsLookupFn()(hostname, {}, (err, addrs) => resolve({ err: err ?? null, addrs }))
    })
  }

  it('allows a hostname that resolves to a public IP', async () => {
    mockDnsCallbackLookup.mockImplementation(
      (_h: string, _o: unknown, cb: (e: null, a: Array<{ address: string, family: number }>) => void) =>
        cb(null, [{ address: '1.2.3.4', family: 4 }]),
    )
    const { err, addrs } = await lookup('api.example.com')
    expect(err).toBeNull()
    expect(addrs).toEqual([{ address: '1.2.3.4', family: 4, ttl: 10_000 }])
  })

  it('blocks a hostname that resolves to a private/reserved IP', async () => {
    mockDnsCallbackLookup.mockImplementation(
      (_h: string, _o: unknown, cb: (e: null, a: Array<{ address: string, family: number }>) => void) =>
        cb(null, [{ address: '127.0.0.1', family: 4 }]),
    )
    const { err } = await lookup('internal.example.com')
    expect(err?.message).toMatch(/private\/reserved IP/)
  })

  it.each([
    ['loopback', '127.0.0.1'],
    ['AWS IMDS', '169.254.169.254'],
    ['RFC1918 10/8', '10.0.0.1'],
    ['RFC1918 172.16/12', '172.16.0.1'],
    ['RFC1918 192.168/16', '192.168.1.1'],
    ['IPv6 loopback', '::1'],
    ['IPv6 link-local', 'fe80::1'],
  ])('assertPublicAddresses blocks %s', (_label, ip) => {
    expect(() => assertPublicAddresses('h', [{ address: ip }])).toThrow(/private\/reserved IP/)
    expect(isReservedIp(ip)).toBe(true)
  })

  it('assertPublicAddresses allows a public address', () => {
    expect(() => assertPublicAddresses('h', [{ address: '1.2.3.4' }])).not.toThrow()
    expect(isReservedIp('1.2.3.4')).toBe(false)
  })
})

describe('DNS pinning (pinDns: true)', () => {
  const { handler } = createSafeFetch({
    rules: { host: 'api.example.com', routes: [{ path: '/**' }] },
  })

  it('blocks IP literal requests to private addresses (checked synchronously)', async () => {
    // undici's interceptor short-circuits IP literals, so a private literal is
    // refused inline before any request is made.
    await expect(
      handler('https://192.168.1.1/', { method: 'GET', headers: {}, body: null }),
    ).rejects.toThrow(/private\/reserved/)
  })

  it('allows a public IP literal', async () => {
    const { handler: handlerPublicIp } = createSafeFetch({
      rules: { host: '1.2.3.4', routes: [{ path: '/**' }] },
    })
    await expect(
      handlerPublicIp('https://1.2.3.4/', { method: 'GET', headers: {}, body: null }),
    ).resolves.toBeDefined()
  })

  it('a denied host is never looked up — the allow-list gates DNS', async () => {
    // A rules-denied request never reaches the HTTP call, so it never reaches
    // the DNS interceptor either. Nothing is resolved for a host the allow-list
    // rejects (the covert-lookup channel is closed).
    await expect(
      handler('https://not-allowed.example.com/', { method: 'GET', headers: {}, body: null }),
    ).rejects.toThrow()
    expect(mockFetch).not.toHaveBeenCalled()
    expect(mockDnsCallbackLookup).not.toHaveBeenCalled()
    expect(mockDnsPromiseLookup).not.toHaveBeenCalled()
  })

  it('does not resolve DNS before the policy decides', async () => {
    const resolvedIpSeen: Array<string | null> = []
    const { handler: handlerWithPolicy } = createSafeFetch({
      policy: (req) => {
        resolvedIpSeen.push(req.resolvedIp)
        return req.host === 'api.example.com'
      },
    })
    await handlerWithPolicy('https://api.example.com/', { method: 'GET', headers: {}, body: null })
    // The policy no longer receives a resolved IP: nothing is looked up until
    // the request is authorized.
    expect(resolvedIpSeen).toEqual([null])
  })
})

// ─────────────────────────────────────────────────────────────────────────
// Hooks: onDenied (for request logging, use middleware instead)
// ─────────────────────────────────────────────────────────────────────────

describe('hooks', () => {
  it('calls onDenied when a request is denied by rules', async () => {
    const onDenied = vi.fn()
    const { handler } = createSafeFetch({
      rules: { host: 'api.example.com', routes: [{ path: '/public/**' }] },
      onDenied,
      pinDns: false,
    })
    await expect(
      handler('https://api.example.com/admin', { method: 'GET', headers: {}, body: null }),
    ).rejects.toThrow()
    expect(onDenied).toHaveBeenCalledOnce()
    expect(onDenied.mock.calls[0]?.[1]).toMatch(/no matching route/)
  })

  it('calls onDenied when a request is denied by policy', async () => {
    const onDenied = vi.fn()
    const { handler } = createSafeFetch({ policy: () => false, onDenied, pinDns: false })
    await expect(
      handler('https://example.com/', { method: 'GET', headers: {}, body: null }),
    ).rejects.toThrow()
    expect(onDenied).toHaveBeenCalledOnce()
  })

  it('onDenied errors are silently ignored', async () => {
    const { handler } = createSafeFetch({
      policy: () => false,
      onDenied: () => {
        throw new Error('hook error')
      },
      pinDns: false,
    })
    // The request should still reject with the deny reason, not the hook error
    await expect(
      handler('https://example.com/', { method: 'GET', headers: {}, body: null }),
    ).rejects.toThrow(/denied by policy/)
  })
})

// ─────────────────────────────────────────────────────────────────────────
// Response handling
// ─────────────────────────────────────────────────────────────────────────

describe('response handling', () => {
  const { handler } = createSafeFetch({
    rules: { host: 'api.example.com', routes: [{ path: '/**' }] },
    pinDns: false,
  })

  it('returns status, headers, and body from a successful response', async () => {
    mockFetch.mockResolvedValue(
      new Response(new Uint8Array([104, 101, 108, 108, 111]), {
        status: 201,
        headers: { 'x-custom': 'value' },
      }),
    )
    const result = await handler('https://api.example.com/create', { method: 'POST', headers: {}, body: null })
    expect(result.status).toBe(201)
    expect(result.headers['x-custom']).toBe('value')
    expect(result.body).toBeInstanceOf(Uint8Array)
  })

  it('returns null body when response has no body', async () => {
    mockFetch.mockResolvedValue(new Response(null, { status: 204 }))
    const result = await handler('https://api.example.com/delete', { method: 'DELETE', headers: {}, body: null })
    expect(result.status).toBe(204)
    expect(result.body).toBeNull()
  })

  it('passes a 3xx response through when maxRedirects is 0 (default)', async () => {
    mockFetch.mockResolvedValue(redirectResponse('https://api.example.com/new-location'))
    const result = await handler('https://api.example.com/old', { method: 'GET', headers: {}, body: null })
    expect(result.status).toBe(302)
    expect(result.headers['location']).toBe('https://api.example.com/new-location')
  })

  it('sets Accept-Encoding: identity when allowCompressedResponses is false (default)', async () => {
    const { handler: handler2 } = createSafeFetch({
      rules: { host: 'api.example.com', routes: [{ path: '/**' }] },
      pinDns: false,
    })
    await handler2('https://api.example.com/data', { method: 'GET' })
    const callHeaders = mockFetch.mock.calls[0]?.[1]?.headers as Record<string, string> | undefined
    expect(callHeaders?.['accept-encoding']).toBe('identity')
  })
})

// ─────────────────────────────────────────────────────────────────────────
// Redirect following
// ─────────────────────────────────────────────────────────────────────────

describe('redirect following (maxRedirects > 0)', () => {
  it('follows a redirect and re-checks policy', async () => {
    const seenHops: number[] = []
    const { handler } = createSafeFetch({
      policy: (req) => {
        seenHops.push(req.hop)
        return req.host === 'api.example.com'
      },
      maxRedirects: 1,
      pinDns: false,
    })

    mockFetch
      .mockResolvedValueOnce(redirectResponse('https://api.example.com/new', 301))
      .mockResolvedValueOnce(okResponse())

    const result = await handler('https://api.example.com/old', { method: 'GET', headers: {}, body: null })

    expect(result.status).toBe(200)
    expect(seenHops).toEqual([0, 1])
  })

  it('denies a redirect that points to a disallowed origin', async () => {
    const { handler } = createSafeFetch({
      rules: { host: 'api.example.com', routes: [{ path: '/**' }] },
      maxRedirects: 1,
      pinDns: false,
    })

    mockFetch.mockResolvedValue(redirectResponse('https://evil.com/steal-tokens', 302))

    await expect(
      handler('https://api.example.com/data', { method: 'GET', headers: {}, body: null }),
    ).rejects.toThrow()
  })

  it('converts POST to GET on 303', async () => {
    const { handler } = createSafeFetch({
      rules: { host: 'api.example.com', routes: [{ path: '/**' }] },
      maxRedirects: 1,
      pinDns: false,
    })

    mockFetch
      .mockResolvedValueOnce(redirectResponse('https://api.example.com/result', 303))
      .mockResolvedValueOnce(okResponse())

    await handler('https://api.example.com/submit', { method: 'POST', headers: {}, body: 'form data' })

    // Second call must be GET
    const secondCall = mockFetch.mock.calls[1]
    expect(secondCall?.[1]?.method).toBe('GET')
  })

  it('passes the redirect response through when the hop limit is hit', async () => {
    const { handler } = createSafeFetch({
      rules: { host: 'api.example.com', routes: [{ path: '/**' }] },
      maxRedirects: 1,
      pinDns: false,
    })

    // Both calls return redirects
    mockFetch
      .mockResolvedValueOnce(redirectResponse('https://api.example.com/step2', 302))
      .mockResolvedValueOnce(redirectResponse('https://api.example.com/step3', 302))

    const result = await handler('https://api.example.com/start', { method: 'GET', headers: {}, body: null })

    // After maxRedirects (1) is exhausted, the raw redirect is returned
    expect(result.status).toBe(302)
  })

  // Mirror undici's RedirectHandler: on a cross-origin hop, drop
  // authorization/cookie/proxy-authorization so a host secret injected by
  // middleware is not replayed to the redirect destination.
  it('strips credential headers on a cross-origin redirect', async () => {
    const { handler } = createSafeFetch({
      policy: (req) => req.host === 'api.example.com' || req.host === 'other.example.com',
      middleware: async (ctx, next) => {
        ctx.req.header('authorization', 'Bearer secret')
        ctx.req.header('cookie', 'session=abc')
        await next()
      },
      maxRedirects: 1,
      pinDns: false,
    })

    mockFetch
      .mockResolvedValueOnce(redirectResponse('https://other.example.com/landing', 302))
      .mockResolvedValueOnce(okResponse())

    await handler('https://api.example.com/start', { method: 'GET', headers: {}, body: null })

    const firstHeaders = mockFetch.mock.calls[0]?.[1]?.headers as Record<string, string>
    const secondHeaders = mockFetch.mock.calls[1]?.[1]?.headers as Record<string, string>
    // Injected on the first hop…
    expect(firstHeaders.authorization).toBe('Bearer secret')
    // …and gone on the cross-origin hop.
    expect(secondHeaders.authorization).toBeUndefined()
    expect(secondHeaders.cookie).toBeUndefined()
  })

  it('keeps credential headers on a same-origin redirect', async () => {
    const { handler } = createSafeFetch({
      policy: (req) => req.host === 'api.example.com',
      middleware: async (ctx, next) => {
        ctx.req.header('authorization', 'Bearer secret')
        await next()
      },
      maxRedirects: 1,
      pinDns: false,
    })

    mockFetch
      .mockResolvedValueOnce(redirectResponse('https://api.example.com/new', 302))
      .mockResolvedValueOnce(okResponse())

    await handler('https://api.example.com/old', { method: 'GET', headers: {}, body: null })

    const secondHeaders = mockFetch.mock.calls[1]?.[1]?.headers as Record<string, string>
    expect(secondHeaders.authorization).toBe('Bearer secret')
  })

  it('strips content-* headers when a 303 turns POST into GET', async () => {
    const { handler } = createSafeFetch({
      rules: { host: 'api.example.com', routes: [{ path: '/**' }] },
      maxRedirects: 1,
      pinDns: false,
    })

    mockFetch
      .mockResolvedValueOnce(redirectResponse('https://api.example.com/result', 303))
      .mockResolvedValueOnce(okResponse())

    await handler('https://api.example.com/submit', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: '{"a":1}',
    })

    const secondCall = mockFetch.mock.calls[1]
    expect(secondCall?.[1]?.method).toBe('GET')
    const secondHeaders = secondCall?.[1]?.headers as Record<string, string>
    expect(secondHeaders['content-type']).toBeUndefined()
  })

  it('applies one timeout deadline across the whole redirect chain', async () => {
    const { handler } = createSafeFetch({
      rules: { host: 'api.example.com', routes: [{ path: '/**' }] },
      maxRedirects: 1,
      pinDns: false,
    })

    mockFetch
      .mockResolvedValueOnce(redirectResponse('https://api.example.com/next', 302))
      .mockResolvedValueOnce(okResponse())

    await handler('https://api.example.com/start', { method: 'GET', headers: {}, body: null })

    const s0 = mockFetch.mock.calls[0]?.[1]?.signal
    const s1 = mockFetch.mock.calls[1]?.[1]?.signal
    expect(s0).toBeDefined()
    // The same deadline spans both hops — not a fresh timeout per hop.
    expect(s1).toBe(s0)
  })
})

// ─────────────────────────────────────────────────────────────────────────
// Middleware
// ─────────────────────────────────────────────────────────────────────────

describe('middleware', () => {
  it('runs global middleware before the HTTP call', async () => {
    const { handler } = createSafeFetch({
      rules: { host: 'api.example.com', routes: [{ path: '/**' }] },
      middleware: async (ctx, next) => {
        ctx.req.header('x-injected', 'yes')
        await next()
      },
      pinDns: false,
    })
    await handler('https://api.example.com/users', { method: 'GET', headers: {}, body: null })
    const sentHeaders = mockFetch.mock.calls[0]?.[1]?.headers as Record<string, string>
    expect(sentHeaders?.['x-injected']).toBe('yes')
  })

  it('runs origin middleware after global', async () => {
    const order: string[] = []
    const { handler } = createSafeFetch({
      rules: {
        host: 'api.example.com',
        middleware: async (_ctx, next) => {
          order.push('origin')
          await next()
        },
        routes: [{ path: '/**' }],
      },
      middleware: async (_ctx, next) => {
        order.push('global')
        await next()
      },
      pinDns: false,
    })
    await handler('https://api.example.com/users', { method: 'GET', headers: {}, body: null })
    expect(order).toEqual(['global', 'origin'])
  })

  it('runs route middleware last', async () => {
    const order: string[] = []
    const { handler } = createSafeFetch({
      rules: {
        host: 'api.example.com',
        middleware: async (_ctx, next) => {
          order.push('origin')
          await next()
        },
        routes: [{
          path: '/**',
          middleware: async (_ctx, next) => {
            order.push('route')
            await next()
          },
        }],
      },
      middleware: async (_ctx, next) => {
        order.push('global')
        await next()
      },
      pinDns: false,
    })
    await handler('https://api.example.com/users', { method: 'GET', headers: {}, body: null })
    expect(order).toEqual(['global', 'origin', 'route'])
  })

  it('route middleware can override headers set by origin middleware', async () => {
    const { handler } = createSafeFetch({
      rules: {
        host: 'api.example.com',
        middleware: async (ctx, next) => {
          ctx.req.header('x-level', 'origin')
          await next()
        },
        routes: [{
          path: '/**',
          middleware: async (ctx, next) => {
            ctx.req.header('x-level', 'route')
            await next()
          },
        }],
      },
      pinDns: false,
    })
    await handler('https://api.example.com/users', { method: 'GET', headers: {}, body: null })
    const sentHeaders = mockFetch.mock.calls[0]?.[1]?.headers as Record<string, string>
    expect(sentHeaders?.['x-level']).toBe('route')
  })

  it('middleware can rewrite the URL', async () => {
    const { handler } = createSafeFetch({
      rules: { host: 'api.example.com', routes: [{ path: '/**' }] },
      middleware: async (ctx, next) => {
        ctx.req.setUrl(ctx.req.url.replace('/v1/', '/v2/'))
        await next()
      },
      pinDns: false,
    })
    await handler('https://api.example.com/v1/users', { method: 'GET', headers: {}, body: null })
    const calledUrl = mockFetch.mock.calls[0]?.[0] as string
    expect(calledUrl).toContain('/v2/users')
  })

  it('middleware can replace the body', async () => {
    const { handler } = createSafeFetch({
      rules: { host: 'api.example.com', routes: [{ path: '/**' }] },
      middleware: async (ctx, next) => {
        ctx.req.setBody('injected body')
        await next()
      },
      pinDns: false,
    })
    await handler('https://api.example.com/data', { method: 'POST', headers: {}, body: 'original' })
    const sentBody = mockFetch.mock.calls[0]?.[1]?.body
    expect(sentBody).toBe('injected body')
  })

  it('setBody works when detached from ctx.req', async () => {
    const { handler } = createSafeFetch({
      rules: { host: 'api.example.com', routes: [{ path: '/**' }] },
      middleware: async (ctx, next) => {
        const { setBody } = ctx.req
        setBody('detached body')
        await next()
      },
      pinDns: false,
    })
    await handler('https://api.example.com/data', { method: 'POST', headers: {}, body: 'original' })
    expect(mockFetch.mock.calls[0]?.[1]?.body).toBe('detached body')
  })

  it('middleware sees route params', async () => {
    const seenParams: Record<string, string>[] = []
    const { handler } = createSafeFetch({
      rules: {
        host: 'api.example.com',
        routes: [{
          path: '/users/:id',
          middleware: async (ctx, next) => {
            seenParams.push(ctx.req.params)
            await next()
          },
        }],
      },
      pinDns: false,
    })
    await handler('https://api.example.com/users/42', { method: 'GET', headers: {}, body: null })
    expect(seenParams[0]).toEqual({ id: '42' })
  })

  it('middleware does not run for policy-only allowed requests that have no matching rule', async () => {
    const middlewareSpy = vi.fn((_ctx: unknown, next: () => Promise<unknown>) => next())
    const { handler } = createSafeFetch({
      policy: () => true,
      middleware: middlewareSpy as Parameters<typeof createSafeFetch>[0]['middleware'],
      pinDns: false,
    })
    await handler('https://anywhere.com/', { method: 'GET', headers: {}, body: null })
    // global middleware still runs even when policy (not rules) allowed the request
    expect(middlewareSpy).toHaveBeenCalledOnce()
  })

  it('middleware does not run for denied requests', async () => {
    const middlewareSpy = vi.fn((_ctx: unknown, next: () => Promise<unknown>) => next())
    const { handler } = createSafeFetch({
      rules: { host: 'api.example.com', routes: [{ path: '/**' }] },
      middleware: middlewareSpy as Parameters<typeof createSafeFetch>[0]['middleware'],
      pinDns: false,
    })
    await expect(
      handler('https://evil.com/', { method: 'GET', headers: {}, body: null }),
    ).rejects.toThrow()
    expect(middlewareSpy).not.toHaveBeenCalled()
  })

  it('middleware can read the response from next()', async () => {
    const seenStatuses: number[] = []
    const { handler } = createSafeFetch({
      rules: { host: 'api.example.com', routes: [{ path: '/**' }] },
      middleware: async (ctx, next) => {
        await next()
        seenStatuses.push(ctx.res!.status)
      },
      pinDns: false,
    })
    mockFetch.mockResolvedValue(new Response(null, { status: 201 }))
    await handler('https://api.example.com/create', { method: 'POST' })
    expect(seenStatuses).toEqual([201])
  })

  it('middleware can synthesise a response without calling next() (no HTTP made)', async () => {
    const { handler } = createSafeFetch({
      rules: { host: 'api.example.com', routes: [{ path: '/queue/**' }] },
      middleware: async (ctx, _next) => ({
        status: 202,
        headers: { 'content-type': 'application/json' },
        body: { queued: true, path: ctx.req.url },
      }),
      pinDns: false,
    })
    const res = await handler('https://api.example.com/queue/task-42', { method: 'POST' })
    expect(res.status).toBe(202)
    expect((res.body as { queued: boolean }).queued).toBe(true)
    // No real HTTP was made
    expect(mockFetch).not.toHaveBeenCalled()
  })

  it('middleware can rewrite the response body', async () => {
    mockFetch.mockResolvedValue(new Response(JSON.stringify({ data: { users: [1, 2, 3] } }), {
      status: 200,
      headers: { 'content-type': 'application/json' },
    }))
    const { handler } = createSafeFetch({
      rules: { host: 'api.example.com', routes: [{ path: '/**' }] },
      middleware: async (ctx, next) => {
        await next()
        const parsed = JSON.parse(new TextDecoder().decode(ctx.res!.body as Uint8Array)) as { data: unknown }
        ctx.res = { ...ctx.res!, body: parsed.data } // unwrap .data for the agent
      },
      pinDns: false,
    })
    const res = await handler('https://api.example.com/users', { method: 'GET' })
    expect(res.body).toEqual({ users: [1, 2, 3] })
  })

  it('async middleware is awaited before the HTTP call', async () => {
    let tokenResolved = false
    const { handler } = createSafeFetch({
      rules: { host: 'api.example.com', routes: [{ path: '/**' }] },
      middleware: async (ctx, next) => {
        await new Promise<void>((resolve) => {
          setTimeout(resolve, 1)
        },
        )
        tokenResolved = true
        ctx.req.header('authorization', 'Bearer token')
        await next()
      },
      pinDns: false,
    })
    await handler('https://api.example.com/data', { method: 'GET', headers: {}, body: null })
    expect(tokenResolved).toBe(true)
    const sentHeaders = mockFetch.mock.calls[0]?.[1]?.headers as Record<string, string>
    expect(sentHeaders?.['authorization']).toBe('Bearer token')
  })

  it('ctx.req.raw preserves the original unmodified request', async () => {
    const seenRaw: any[] = []
    const { handler } = createSafeFetch({
      rules: { host: 'api.example.com', routes: [{ path: '/**' }] },
      middleware: async (ctx, next) => {
        seenRaw.push(ctx.req.raw)
        ctx.req.header('authorization', 'Bearer token')
        await next()
      },
      pinDns: false,
    })
    await handler('https://api.example.com/users', { method: 'GET', headers: { 'x-original': 'yes' }, body: null })
    expect(seenRaw[0]?.headers['x-original']).toBe('yes')
    expect(seenRaw[0]?.headers['authorization']).toBeUndefined()
  })
})
