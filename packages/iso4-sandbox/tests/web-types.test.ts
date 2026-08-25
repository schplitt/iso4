/**
 * End-to-end tests for `Request`/`Response`/`Headers` crossing the boundary.
 *
 * These run against the real Rust binary, so they are the only place the two
 * copies of the wire format (`native/v8-runtime/src/webcodec.rs` and
 * `src/web-codec.ts`) are checked against each other. Unit tests on either side
 * can agree with themselves and still be incompatible.
 */

import { afterAll, beforeAll, describe, expect, test } from 'vitest'

import { createSandbox } from '../src/index'
import type { Sandbox } from '../src/index'

let sandbox: Sandbox

beforeAll(async () => {
  sandbox = await createSandbox({})
}, 30_000)

afterAll(async () => {
  await sandbox?.dispose?.()
})

/**
 * Run `code` and return the `default` export, failing loudly on error.
 * @param code sandbox module source
 * @param globals optional host globals for the run
 */
async function run(code: string, globals?: Record<string, unknown>): Promise<unknown> {
  const result = await sandbox.run(
    globals === undefined ? { code } : { code, globals: globals as never },
  )
  if (!result.ok)
    throw new Error(`${result.error.code}: ${result.error.message}`)
  return result.exports.default
}

describe('sandbox → host', () => {
  test('a Response arrives as a real Response', async () => {
    const value = await run(`
      export default new Response('hello', {
        status: 201,
        statusText: 'Created',
        headers: { 'content-type': 'text/plain' },
      })
    `)
    expect(value).toBeInstanceOf(Response)
    const res = value as Response
    expect(res.status).toBe(201)
    expect(res.statusText).toBe('Created')
    expect(res.headers.get('content-type')).toBe('text/plain')
    await expect(res.text()).resolves.toBe('hello')
  })

  test('duplicate set-cookie survives — a Record could not carry this', async () => {
    const value = await run(`
      const r = new Response(null)
      r.headers.append('set-cookie', 'a=1')
      r.headers.append('set-cookie', 'b=2')
      export default r
    `)
    expect((value as Response).headers.getSetCookie()).toEqual(['a=1', 'b=2'])
  })

  test('a binary body survives byte for byte', async () => {
    const value = await run(
      `export default new Response(new Uint8Array([0, 1, 127, 128, 254, 255]))`,
    )
    const bytes = new Uint8Array(await (value as Response).arrayBuffer())
    expect([...bytes]).toEqual([0, 1, 127, 128, 254, 255])
  })

  test('a Request arrives as a real Request', async () => {
    const value = await run(`
      export default new Request('https://example.com/api?q=1', {
        method: 'POST',
        body: 'payload',
        headers: { 'x-custom': 'v' },
      })
    `)
    expect(value).toBeInstanceOf(Request)
    const req = value as Request
    expect(req.method).toBe('POST')
    expect(req.url).toBe('https://example.com/api?q=1')
    expect(req.headers.get('x-custom')).toBe('v')
    await expect(req.text()).resolves.toBe('payload')
  })

  test('Headers cross standalone', async () => {
    const value = await run(`export default new Headers([['x-a', '1'], ['content-type', 'b']])`)
    expect(value).toBeInstanceOf(Headers)
    expect((value as Headers).get('content-type')).toBe('b')
  })

  test('host types nest at any depth — Rust routes on internal fields', async () => {
    const value = await run(`
      export default { meta: 'm', list: [new Response('x', { status: 202 })] }
    `) as { meta: string, list: Response[] }
    expect(value.meta).toBe('m')
    expect(value.list[0]).toBeInstanceOf(Response)
    expect(value.list[0]!.status).toBe(202)
  })

  test('header names round-trip verbatim, whatever their shape', async () => {
    // Header names are not special-cased, so this checks that arbitrary names
    // survive the flat-array representation both codecs use.
    const names = [
      'content-type',
      'x-custom-thing',
      'a',
      'x'.repeat(200),
      'x-tRaCe-Id',
      'if-modified-since',
      'x-1234',
    ]
    const value = await run(`
      const r = new Response(null)
      for (const [n, v] of ${JSON.stringify(names.map((n) => [n, `v-${n}`]))}) r.headers.append(n, v)
      export default r
    `)
    const headers = (value as Response).headers
    for (const name of names)
      expect(headers.get(name), `header ${name}`).toBe(`v-${name}`)
  })

  test('ordinary values are unaffected by the host-object path', async () => {
    const value = await run(`
      export default { d: new Date(1700000000000), m: new Map([['k', 1]]), b: 10n,
                       t: new Uint8Array([1, 2]), s: new Set([3]) }
    `) as Record<string, unknown>
    expect(value.d).toBeInstanceOf(Date)
    expect((value.m as Map<string, number>).get('k')).toBe(1)
    expect(value.b).toBe(10n)
    expect([...(value.t as Uint8Array)]).toEqual([1, 2])
    expect([...(value.s as Set<number>)]).toEqual([3])
  })
})

describe('host → sandbox', () => {
  test('a Request passed as a data global arrives as a real Request', async () => {
    const request = new Request('https://example.com/in?x=1', {
      method: 'POST',
      body: 'from-host',
      headers: { 'content-type': 'text/plain', 'x-trace': 'abc' },
    })
    const value = await run(
      `export default [
         globalThis.req instanceof Request,
         globalThis.req.method,
         globalThis.req.url,
         globalThis.req.headers.get('x-trace'),
       ].join('|')`,
      { req: { kind: 'data', value: request } },
    )
    expect(value).toBe('true|POST|https://example.com/in?x=1|abc')
  })

  test('the body of an inbound Request is readable in the sandbox', async () => {
    const request = new Request('https://example.com/in', {
      method: 'POST',
      body: JSON.stringify({ n: 7 }),
      headers: { 'content-type': 'application/json' },
    })
    const value = await run(
      `export default (await globalThis.req.json()).n`,
      { req: { kind: 'data', value: request } },
    )
    expect(value).toBe(7)
  })

  test('a Response returned by a bridge handler arrives as a real Response', async () => {
    const value = await run(
      `const r = await globalThis.callHost()
       export default [r instanceof Response, r.status, await r.text()].join('|')`,
      {
        callHost: () => new Response('from-host', { status: 203 }),
      },
    )
    expect(value).toBe('true|203|from-host')
  })
})

describe('host → sandbox nesting', () => {
  test('a Response nested inside a data global becomes a real Response', async () => {
    const value = await run(
      `const r = globalThis.payload.deep.list[0]
       export default [r instanceof Response, r.status, await r.text()].join('|')`,
      {
        payload: {
          kind: 'data',
          value: { deep: { list: [new Response('nested', { status: 207 })] } },
        },
      },
    )
    expect(value).toBe('true|207|nested')
  })

  test('a bridge handler may return a nested Response', async () => {
    const value = await run(
      `const { res, meta } = await globalThis.callHost()
       export default [meta, res instanceof Response, res.status].join('|')`,
      { callHost: () => ({ meta: 'm', res: new Response(null, { status: 204 }) }) },
    )
    expect(value).toBe('m|true|204')
  })
})

describe('refusals', () => {
  test('a non-serializable body reports ERR_TYPE_NOT_SERIALIZABLE', async () => {
    // `_b` is reachable from sandbox code, so this is the codec's own gate
    // rather than the constructor's.
    const result = await sandbox.run({
      code: `const r = new Response('x')
             Object.defineProperty(r, '_b', { value: new DataView(new ArrayBuffer(2)) })
             export default r`,
    })
    expect(result.ok).toBe(false)
    if (!result.ok) {
      expect(result.error.code).toBe('ERR_TYPE_NOT_SERIALIZABLE')
      expect(result.error.message).toMatch(/null, a string, or a Uint8Array/)
    }
  })

  test('too many headers reports ERR_TYPE_NOT_SERIALIZABLE', async () => {
    const result = await sandbox.run({
      code: `const r = new Response(null)
             for (let i = 0; i < 1025; i++) r.headers.append('x-h' + i, 'v')
             export default r`,
    })
    expect(result.ok).toBe(false)
    if (!result.ok)
      expect(result.error.code).toBe('ERR_TYPE_NOT_SERIALIZABLE')
  })

  test('a reserved global name is rejected', async () => {
    const result = await sandbox.run({
      code: `export default 1`,
      globals: { Response: { kind: 'data', value: 1 } } as never,
    })
    expect(result.ok).toBe(false)
    if (!result.ok)
      expect(result.error.code).toBe('ERR_UNDECLARED_BINDING')
  })
})

describe('platform parity — the sandbox throws where Node throws', () => {
  test.each([
    ['status below the range', `new Response(null, { status: 101 })`],
    ['status above the range', `new Response(null, { status: 600 })`],
    ['non-ByteString statusText', `new Response(null, { statusText: '世' })`],
    ['statusText with a newline', `new Response(null, { statusText: 'a\\nb' })`],
    ['non-ByteString header value', `new Response(null, { headers: { 'x-a': '世' } })`],
    ['forbidden method', `new Request('https://ex.com/', { method: 'CONNECT' })`],
  ])('%s is a user-code error, not a host throw', async (_label, expr) => {
    const result = await sandbox.run({ code: `export default ${expr}` })
    expect(result.ok).toBe(false)
    if (!result.ok)
      expect(result.error.code).toBe('ERR_USER_CODE')
  })

  test('Response.error() survives the crossing', async () => {
    const value = await run(`export default Response.error()`)
    expect(value).toBeInstanceOf(Response)
    expect((value as Response).status).toBe(0)
    expect((value as Response).type).toBe('error')
  })

  test('a consumed body does not come back to life', async () => {
    const value = await run(
      `const r = new Response('secret')
       await r.text()
       export default r`,
    )
    expect((value as Response).bodyUsed).toBe(false)
    await expect((value as Response).text()).resolves.toBe('')
  })

  // The sandbox validates header content at construction, but the backing
  // `_l` slot is writable, so guest code can plant a CR/LF value that skips
  // that check. iso4 does not re-validate header *content* at the boundary;
  // the host reconstruction (`new globalThis.Headers` via undici) does, so a
  // smuggled value is rejected on the way in rather than reaching the network.
  // The rejection surfaces as a decode error from `run()`, and the sandbox
  // stays usable afterwards. This pins that containment and backs the note in
  // `@iso4/fetch`'s README.
  test('a header value smuggled past sandbox validation is rejected on the host, not forwarded', async () => {
    await expect(
      sandbox.run({
        code: `
          const r = new Response('x')
          r.headers._l = ['x-injected', 'a\\r\\nevil: 1']
          export default r
        `,
      }),
    ).rejects.toThrow(/invalid header value/i)

    // The failure did not desync the connection — the next run works.
    const after = await run(`export default new Response('ok').status`)
    expect(after).toBe(200)
  })
})
