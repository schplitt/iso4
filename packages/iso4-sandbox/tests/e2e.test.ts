/**
 * End-to-end tests for \@iso4/sandbox.
 *
 * These tests exercise the full stack: TypeScript host → Rust IPC → V8 →
 * Rust IPC → TypeScript host. They are the source of truth for what the
 * runtime must do when Phase 1+ lands.
 *
 * All tests currently fail with "not yet implemented" because createSandbox()
 * is a stub. That is intentional — this is the TDD contract. Un-stub
 * createRuntime() and each group of tests becomes the acceptance criterion
 * for that phase.
 *
 * Test organisation:
 * 1. Direct run (no precompile)
 * 2. Precompile + prefix.run()
 * 3. Globals — fetch handler and custom tool functions
 * 4. Source imports — host-provided JS libraries (e.g. zod-like)
 * 5. Host imports — host-provided function bridges
 * 6. Console / logging
 * 7. Error handling
 * 8. Resource limits
 */

import { afterAll, beforeAll, describe, expect, test } from 'vitest'
import { createSandbox as createRuntime } from '../src/index'
import type { HostExportData, Sandbox as Runtime } from '../src/types'

// ─────────────────────────────────────────────────────────────────────────────
// Shared mock libraries (host-provided JS source strings)
// ─────────────────────────────────────────────────────────────────────────────

/**
 * Minimal zod-like validation library the host can provide as a source module.
 * Real zod would just be the compiled bundle passed as source — same mechanic.
 */
const ZOD_LIKE_SOURCE = /* js */ `
export const z = {
  string: () => ({ _type: 'string' }),
  number: () => ({ _type: 'number' }),
  boolean: () => ({ _type: 'boolean' }),
  object: (shape) => ({
    _type: 'object',
    parse(data) {
      if (typeof data !== 'object' || data === null) {
        throw new Error('expected object, got ' + typeof data)
      }
      for (const key of Object.keys(shape)) {
        if (!(key in data)) throw new Error('missing required key: ' + key)
      }
      return data
    },
    safeParse(data) {
      try { return { success: true, data: this.parse(data) } }
      catch (e) { return { success: false, error: e.message } }
    },
  }),
  array: (item) => ({
    _type: 'array',
    parse(data) {
      if (!Array.isArray(data)) throw new Error('expected array')
      return data
    },
  }),
}
`

/**
 * Minimal math utility library.
 */
const MATH_UTILS_SOURCE = /* js */ `
export const add = (a, b) => a + b
export const multiply = (a, b) => a * b
export const clamp = (v, min, max) => Math.min(Math.max(v, min), max)
export const sum = (...nums) => nums.reduce((a, b) => a + b, 0)
export const lerp = (a, b, t) => a + (b - a) * t
`

/**
 * A tiny agent-style "tools" library that wraps host bridge functions.
 * In production this would be generated from a tool manifest.
 */
const TOOLS_WRAPPER_SOURCE = /* js */ `
import { _search, _weather } from 'host:tools-bridge'

export async function search(query, opts = {}) {
  const raw = await _search(query, opts.limit ?? 5)
  return JSON.parse(raw)
}

export async function getWeather(city) {
  return JSON.parse(await _weather(city))
}
`

// ─────────────────────────────────────────────────────────────────────────────
// 1. Direct run — runtime.run()
// ─────────────────────────────────────────────────────────────────────────────

describe('runtime.run() — direct execution', () => {
  let runtime: Runtime

  beforeAll(async () => {
    runtime = await createRuntime()
  })

  afterAll(async () => {
    await runtime?.dispose()
  })

  test('export default number', async () => {
    const result = await runtime.run({ code: 'export default 42' })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports.default).toBe(42)
  })

  test('export default string', async () => {
    const result = await runtime.run({ code: 'export default "hello"' })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports.default).toBe('hello')
  })

  test('export default boolean', async () => {
    const result = await runtime.run({ code: 'export default true' })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports.default).toBe(true)
  })

  test('export default null', async () => {
    const result = await runtime.run({ code: 'export default null' })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports.default).toBeNull()
  })

  test('export default object', async () => {
    const result = await runtime.run({ code: 'export default { x: 1, y: 2 }' })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports.default).toEqual({ x: 1, y: 2 })
  })

  test('export default array', async () => {
    const result = await runtime.run({ code: 'export default [1, 2, 3]' })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports.default).toEqual([1, 2, 3])
  })

  test('named exports', async () => {
    const result = await runtime.run({
      code: 'export const x = 10; export const y = 20',
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports.x).toBe(10)
    expect(result.exports.y).toBe(20)
  })

  test('default and named exports together', async () => {
    const result = await runtime.run({
      code: 'export default 99; export const label = "hi"',
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports.default).toBe(99)
    expect(result.exports.label).toBe('hi')
  })

  test('top-level await resolves', async () => {
    const result = await runtime.run({
      code: 'export default await Promise.resolve(7)',
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports.default).toBe(7)
  })

  test('async computation resolves', async () => {
    const result = await runtime.run({
      code: `
        async function compute() { return 1 + 1 }
        export default await compute()
      `,
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports.default).toBe(2)
  })

  test('duration is populated', async () => {
    const result = await runtime.run({ code: 'export default 1' })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.durationMs).toBeGreaterThanOrEqual(0)
    expect(result.durationMs).toBeLessThan(5000)
  })

  test('multiple concurrent runs are independent', async () => {
    const [a, b, c] = await Promise.all([
      runtime.run({ code: 'export default 1' }),
      runtime.run({ code: 'export default 2' }),
      runtime.run({ code: 'export default 3' }),
    ])
    expect(a.ok && a.exports.default).toBe(1)
    expect(b.ok && b.exports.default).toBe(2)
    expect(c.ok && c.exports.default).toBe(3)
  })
})

// ─────────────────────────────────────────────────────────────────────────────
// 2. Precompile + prefix.run()
// ─────────────────────────────────────────────────────────────────────────────

describe('precompile + prefix.run()', () => {
  let runtime: Runtime

  beforeAll(async () => {
    runtime = await createRuntime()
  })

  afterAll(async () => {
    await runtime?.dispose()
  })

  test('prefix compiles and postfix runs', async () => {
    // ESM top-level const is module-scoped; use globalThis to share across modules.
    const prefix = await runtime.precompile({
      code: 'globalThis.base = 100',
    })
    const result = await prefix.run({ code: 'export default globalThis.base + 1' })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports.default).toBe(101)
    await prefix.dispose()
  })

  test('prepare()/execute() are aliases of precompile()/run()', async () => {
    // The canonical names must behave identically to the deprecated aliases,
    // and interoperate (a prefix from prepare() still answers run()).
    const prefix = await runtime.prepare({ code: 'globalThis.base = 200' })
    const viaExecute = await prefix.execute({ code: 'export default globalThis.base + 1' })
    expect(viaExecute.ok).toBe(true)
    if (!viaExecute.ok)
      return
    expect(viaExecute.exports.default).toBe(201)
    // Deprecated run() still works on a prefix produced by prepare().
    const viaRun = await prefix.run({ code: 'export default globalThis.base + 2' })
    expect(viaRun.ok).toBe(true)
    if (!viaRun.ok)
      return
    expect(viaRun.exports.default).toBe(202)
    await prefix.dispose()
  })

  test('many postfix runs against the same prefix', async () => {
    const prefix = await runtime.precompile({
      code: 'globalThis.multiplier = 10',
    })

    const results = await Promise.all(
      [1, 2, 3, 4, 5].map((n) =>
        prefix.run({ code: `export default ${n} * globalThis.multiplier` }),
      ),
    )

    for (const [i, result] of results.entries()) {
      expect(result.ok).toBe(true)
      if (!result.ok)
        continue
      expect(result.exports.default).toBe((i + 1) * 10)
    }

    await prefix.dispose()
  })

  test('warm reuse: a later run may see an earlier run\'s globals', async () => {
    const prefix = await runtime.precompile({ code: '' })

    // Prefix runs reuse warm instances of the same prefix, so a
    // mutation made by one run MAY be visible to the next — permitted,
    // unguaranteed, evictable at any time (warmth is a cache). Back-to-back
    // runs on one prefix reuse deterministically, so assert the carryover.
    await prefix.run({ code: 'globalThis.__secret = 42; export default 1' })
    const second = await prefix.run({
      code: 'export default globalThis.__secret ?? "clean"',
    })

    expect(second.ok).toBe(true)
    if (!second.ok)
      return
    expect(second.exports.default).toBe(42)

    await prefix.dispose()
  })

  test('prefix with source library available in postfix', async () => {
    const prefix = await runtime.precompile({
      code: `import { add } from 'lib:math'; globalThis.add = add`,
      imports: {

        'lib:math': MATH_UTILS_SOURCE,
      },
    })

    const result = await prefix.run({ code: 'export default add(3, 4)' })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports.default).toBe(7)

    await prefix.dispose()
  })

  test('prefix with top-level await settles and snapshots', async () => {
    const prefix = await runtime.precompile({
      code: `globalThis.a = await Promise.resolve(41)
globalThis.b = await new Response('body').text()`,
    })
    const result = await prefix.run({
      code: 'export default [globalThis.a + 1, globalThis.b].join("|")',
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports.default).toBe('42|body')
    await prefix.dispose()
  })

  test('prefix with top-level await in plain exports settles (bundled-handler shape)', async () => {
    // A prefix's module namespace is discarded by design, so these bindings
    // are unobservable from a postfix — the point is that prepare() itself
    // succeeds for the shapes bundled handlers commonly produce.
    const prefix = await runtime.precompile({
      code: `export const test = await 1
export default await Promise.resolve(2)`,
    })
    const result = await prefix.run({ code: 'export default "ran"' })
    expect(result.ok).toBe(true)
    await prefix.dispose()
  })

  test('prefix importing a module with top-level await settles', async () => {
    const prefix = await runtime.precompile({
      code: `import { config } from 'lib:config'
globalThis.retries = config.retries`,
      imports: {
        'lib:config': 'export const config = await Promise.resolve({ retries: 3 })',
      },
    })
    const result = await prefix.run({ code: 'export default globalThis.retries' })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports.default).toBe(3)
    await prefix.dispose()
  })

  test('prefix that never settles rejects with ERR_PREFIX_DID_NOT_SETTLE', async () => {
    await expect(
      runtime.precompile({ code: 'await new Promise(() => {})' }),
    ).rejects.toMatchObject({ code: 'ERR_PREFIX_DID_NOT_SETTLE' })
  })

  test('prefix calling a declared bridge global rejects with ERR_PREFIX_BRIDGE_CALL', async () => {
    // No host session exists at prepare() time — the declared global is
    // visible (typeof matches run() code) but calling it must fail with the
    // dedicated code, not a misleading ReferenceError.
    await expect(
      runtime.precompile({
        code: 'await fetch("https://example.com")',
        globals: { fetch: async () => ({ status: 200 }) },
      }),
    ).rejects.toMatchObject({ code: 'ERR_PREFIX_BRIDGE_CALL' })
  })

  test('prefix calling a host-import function rejects with ERR_PREFIX_BRIDGE_CALL', async () => {
    await expect(
      runtime.precompile({
        code: `import { search } from 'tools:web'
await search('dogs')`,
        imports: {
          'tools:web': { search: async () => [] },
        },
      }),
    ).rejects.toMatchObject({ code: 'ERR_PREFIX_BRIDGE_CALL' })
  })

  test('prefix.alive is false after dispose', async () => {
    const prefix = await runtime.precompile({ code: '' })
    expect(prefix.alive).toBe(true)
    await prefix.dispose()
    expect(prefix.alive).toBe(false)
  })

  test('run on disposed prefix rejects', async () => {
    const prefix = await runtime.precompile({ code: '' })
    await prefix.dispose()
    const result = await prefix.run({ code: 'export default 1' })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.code).toBe('ERR_PREFIX_DISPOSED')
  })
})

// ─────────────────────────────────────────────────────────────────────────────
// 3. Globals — fetch and custom tool functions
// ─────────────────────────────────────────────────────────────────────────────

describe('globals', () => {
  let runtime: Runtime

  beforeAll(async () => {
    runtime = await createRuntime()
  })

  afterAll(async () => {
    await runtime?.dispose()
  })

  test('fetch without configuration is just a missing global/user-code error', async () => {
    const result = await runtime.run({
      code: 'export default await fetch("https://example.com")',
    })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.code).toBe('ERR_USER_CODE')
  })

  test('fetch with handler receives the request and returns response', async () => {
    // fetch is a generic bridge global: handler receives raw args (url string).
    // The handler returns a plain data object; sandbox accesses fields directly.
    const requests: string[] = []

    const result = await runtime.run({
      code: `
        const res = await fetch('https://api.example.com/data')
        const json = JSON.parse(res.body)
        export default json.value
      `,
      globals: {
        fetch: async (url: unknown) => {
          requests.push(url as string)
          return {
            status: 200,
            headers: { 'content-type': 'application/json' },
            body: JSON.stringify({ value: 42 }),
          }
        },
      },
    })

    expect(requests).toEqual(['https://api.example.com/data'])
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports.default).toBe(42)
  })

  test('fetch handler can deny a request', async () => {
    const result = await runtime.run({
      code: `
        const res = await fetch('https://denied.example.com')
        export default res.status
      `,
      globals: {
        fetch: async () => ({ status: 403, headers: {}, body: 'forbidden' }),
      },
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports.default).toBe(403)
  })

  test('per-run fetch handler is scoped to that run', async () => {
    const calls: string[] = []

    const prefix = await runtime.precompile({
      code: `
        globalThis.run = async (url) => {
          const r = await fetch(url)
          return r.status
        }
      `,
      // Declare fetch at precompile time so the prefix body can reference it.
      globals: { fetch: async () => ({ status: 200, headers: {}, body: null }) },
    })

    await prefix.run({
      code: 'export default await globalThis.run("https://run-a.example.com")',
      globals: {
        fetch: async (url: unknown) => {
          calls.push(`a:${url as string}`)
          return { status: 200, headers: {}, body: null }
        },
      },
    })

    await prefix.run({
      code: 'export default await globalThis.run("https://run-b.example.com")',
      globals: {
        fetch: async (url: unknown) => {
          calls.push(`b:${url as string}`)
          return { status: 200, headers: {}, body: null }
        },
      },
    })

    expect(calls).toEqual([
      'a:https://run-a.example.com',
      'b:https://run-b.example.com',
    ])

    await prefix.dispose()
  })
})

// ─────────────────────────────────────────────────────────────────────────────
// 4. Source imports — host-provided JS libraries
// ─────────────────────────────────────────────────────────────────────────────

describe('source imports', () => {
  let runtime: Runtime

  beforeAll(async () => {
    runtime = await createRuntime()
  })

  afterAll(async () => {
    await runtime?.dispose()
  })

  test('import from host-provided source module', async () => {
    const result = await runtime.run({
      code: `
        import { add } from 'lib:math'
        export default add(3, 4)
      `,
      imports: {
        'lib:math': MATH_UTILS_SOURCE,
      },
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports.default).toBe(7)
  })

  test('import multiple functions from source module', async () => {
    const result = await runtime.run({
      code: `
        import { clamp, sum, lerp } from 'lib:math'
        export default sum(clamp(5, 0, 10), lerp(0, 100, 0.5))
      `,
      imports: {
        'lib:math': MATH_UTILS_SOURCE,
      },
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports.default).toBe(55) // clamp(5,0,10)=5, lerp(0,100,0.5)=50 → 55
  })

  test('zod-like schema validation — happy path', async () => {
    const result = await runtime.run({
      code: `
        import { z } from 'lib:zod'
        const schema = z.object({ name: z.string(), age: z.number() })
        export default schema.parse({ name: 'Alice', age: 30 })
      `,
      imports: {
        'lib:zod': ZOD_LIKE_SOURCE,
      },
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports.default).toEqual({ name: 'Alice', age: 30 })
  })

  test('zod-like schema validation — missing key throws', async () => {
    const result = await runtime.run({
      code: `
        import { z } from 'lib:zod'
        const schema = z.object({ name: z.string() })
        export default schema.parse({ wrong: true })
      `,
      imports: {
        'lib:zod': ZOD_LIKE_SOURCE,
      },
    })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.code).toBe('ERR_USER_CODE')
    expect(result.error.message).toContain('missing required key')
  })

  test('zod-like safeParse returns error shape instead of throwing', async () => {
    const result = await runtime.run({
      code: `
        import { z } from 'lib:zod'
        const schema = z.object({ name: z.string() })
        export default schema.safeParse({ wrong: true })
      `,
      imports: {
        'lib:zod': ZOD_LIKE_SOURCE,
      },
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports.default).toMatchObject({ success: false })
  })

  test('source module declared on prefix is reachable from postfix import', async () => {
    // ESM bindings don't cross module boundaries: postfix code that wants a
    // source-module function must import it itself. The precompiled prefix's
    // role is to declare the binding so the postfix's import resolves
    // against the same source. (DESIGN.md §4.3.)
    const prefix = await runtime.precompile({
      code: `import { add, multiply } from 'lib:math'`,
      imports: {
        'lib:math': MATH_UTILS_SOURCE,
      },
    })

    const result = await prefix.run({
      code: `import { add, multiply } from 'lib:math'
             export default multiply(add(1, 2), 4)`,
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports.default).toBe(12)

    await prefix.dispose()
  })

  test('unknown import specifier returns ERR_MODULE_NOT_FOUND', async () => {
    const result = await runtime.run({
      code: 'import { x } from "not:registered"; export default x',
    })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.code).toBe('ERR_MODULE_NOT_FOUND')
  })
})

// ─────────────────────────────────────────────────────────────────────────────
// 5. Host imports — bridge functions provided by the host
// ─────────────────────────────────────────────────────────────────────────────

describe('host imports', () => {
  let runtime: Runtime

  beforeAll(async () => {
    runtime = await createRuntime()
  })

  afterAll(async () => {
    await runtime?.dispose()
  })

  test('host function is callable from sandbox', async () => {
    const calls: unknown[] = []
    const result = await runtime.run({
      code: `
        import { echo } from 'host:utils'
        export default await echo('ping')
      `,
      imports: {

        'host:utils': {
          echo: (msg) => {
            calls.push(msg)
            return String(msg).toUpperCase()
          },
        },
      },
    })
    expect(calls).toEqual(['ping'])
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports.default).toBe('PING')
  })

  test('widened data leaves arrive as real instances, cycles included', async () => {
    // `HostExportData` covers everything V8's format carries, and nothing on
    // the host walks a data leaf to check it — so these reach the sandbox as
    // real instances, not as flattened plain objects.
    const cyclic: { name: string, self?: unknown } = { name: 'root' }
    cyclic.self = cyclic
    const result = await runtime.run({
      code: `
        import { when, pattern, failure, lookup, tags, floats, loop } from 'host:data'
        export default {
          date: when instanceof Date && when.getTime(),
          regexp: pattern instanceof RegExp && pattern.source + '/' + pattern.flags,
          error: failure instanceof TypeError && failure.message,
          map: lookup instanceof Map && lookup.get('a'),
          set: tags instanceof Set && tags.has('x'),
          typed: floats instanceof Float64Array && floats[1],
          cycleHeld: loop[0].self === loop[0] && loop[0].name,
        }
      `,
      imports: {
        'host:data': {
          when: new Date(1700000000000),
          pattern: /ab+c/gi,
          failure: new TypeError('boom'),
          lookup: new Map<unknown, unknown>([['a', 1]]),
          tags: new Set(['x']),
          floats: new Float64Array([1.5, -2.5]),
          // The cycle sits inside an array: an array is a data leaf and is
          // never walked, so the back-reference survives. A cycle through a
          // *plain* object still throws — the shape walker has to descend
          // those to find function leaves (see DESIGN.md §4.3).
          loop: [cyclic],
        },
      },
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports.default).toEqual({
      date: 1700000000000,
      regexp: 'ab+c/gi',
      error: 'boom',
      map: 1,
      set: true,
      typed: -2.5,
      cycleHeld: 'root',
    })
  })

  test('async host function is awaited correctly', async () => {
    const result = await runtime.run({
      code: `
        import { fetchData } from 'host:api'
        export default await fetchData('users')
      `,
      imports: {

        'host:api': {
          fetchData: async (resource: unknown) => {
            // simulate async work
            return `data for ${resource}`
          },
        },
      },
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports.default).toBe('data for users')
  })

  test('host module tools bridge called via source wrapper', async () => {
    const searchResults = [{ title: 'result 1' }, { title: 'result 2' }]
    const result = await runtime.run({
      code: `
        import { search } from 'lib:tools'
        const results = await search('cats')
        export default results.length
      `,
      imports: {

        'lib:tools': TOOLS_WRAPPER_SOURCE,
        'host:tools-bridge': {
          _search: async (query: unknown, limit: unknown) =>
            JSON.stringify(searchResults.slice(0, Number(limit))),
          _weather: async () => JSON.stringify({ temp: 20 }),
        },
      },
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports.default).toBe(2)
  })

  test('host function receives correct argument types', async () => {
    const received: unknown[] = []
    await runtime.run({
      code: `
        import { record } from 'host:spy'
        await record(42, true, 'hello', null)
        export default 1
      `,
      imports: {

        'host:spy': {
          record: (...args: unknown[]) => {
            received.push(...args)
            return null
          },
        },
      },
    })
    expect(received).toEqual([42, true, 'hello', null])
  })

  test('host function passed a function argument fails with ERR_FUNCTION_ARGUMENT_NOT_SUPPORTED', async () => {
    const result = await runtime.run({
      code: `
        import { call } from 'host:cb'
        export default await call(() => 42)
      `,
      imports: {

        'host:cb': { call: (fn: unknown) => fn },
      },
    })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.code).toBe('ERR_FUNCTION_ARGUMENT_NOT_SUPPORTED')
  })
})

// ─────────────────────────────────────────────────────────────────────────────
// 6. Console / logging
// ─────────────────────────────────────────────────────────────────────────────

describe('console capture', () => {
  let runtime: Runtime

  beforeAll(async () => {
    runtime = await createRuntime()
  })

  afterAll(async () => {
    await runtime?.dispose()
  })

  test('console.log captured in stdout', async () => {
    const result = await runtime.run({
      code: 'console.log("hello"); export default 1',
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.stdout).toContain('hello')
  })

  test('console.error captured in stderr', async () => {
    const result = await runtime.run({
      code: 'console.error("oops"); export default 1',
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.stderr).toContain('oops')
  })

  test('console.log does not leak into stderr', async () => {
    const result = await runtime.run({
      code: 'console.log("stdout only"); export default 1',
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.stderr).toEqual([])
  })

  test('multiple console.log calls all captured', async () => {
    const result = await runtime.run({
      code: `
        console.log("line one")
        console.log("line two")
        console.log("line three")
        export default 1
      `,
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.stdout).toContain('line one')
    expect(result.stdout).toContain('line two')
    expect(result.stdout).toContain('line three')
  })

  test('console.log does not appear in a different run', async () => {
    await runtime.run({ code: 'console.log("run-a output"); export default 1' })
    const second = await runtime.run({ code: 'export default 1' })
    expect(second.ok).toBe(true)
    if (!second.ok)
      return
    expect(second.stdout).toEqual([])
  })
})

// ─────────────────────────────────────────────────────────────────────────────
// 7. Error handling
// ─────────────────────────────────────────────────────────────────────────────

describe('error handling', () => {
  let runtime: Runtime

  beforeAll(async () => {
    runtime = await createRuntime()
  })

  afterAll(async () => {
    await runtime?.dispose()
  })

  test('syntax error → ERR_COMPILE', async () => {
    const result = await runtime.run({ code: 'export default (((' })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.code).toBe('ERR_COMPILE')
  })

  test('thrown Error → ERR_USER_CODE with message', async () => {
    const result = await runtime.run({
      code: 'throw new Error("something went wrong")',
    })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.code).toBe('ERR_USER_CODE')
    expect(result.error.message).toContain('something went wrong')
  })

  test('error has a stack trace', async () => {
    const result = await runtime.run({
      code: `
        function inner() { throw new Error("deep") }
        function outer() { inner() }
        outer()
      `,
    })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.stack).toBeTruthy()
    expect(result.error.stack).toContain('inner')
  })

  test('exporting a function is skipped and reported, not fatal', async () => {
    const result = await runtime.run({
      code: 'export default function() {}\nexport const kept = 1',
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.skippedExports).toEqual(['default'])
    expect('default' in result.exports).toBe(false)
    expect(result.exports['kept']).toBe(1)
  })

  test('error in one run does not affect the next run', async () => {
    const bad = await runtime.run({ code: 'throw new Error("fail")' })
    expect(bad.ok).toBe(false)

    const good = await runtime.run({ code: 'export default 42' })
    expect(good.ok).toBe(true)
    if (!good.ok)
      return
    expect(good.exports.default).toBe(42)
  })

  test('TypeError propagates name', async () => {
    const result = await runtime.run({ code: 'throw new TypeError("bad type")' })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.code).toBe('ERR_USER_CODE')
    expect(result.error.name).toBe('TypeError')
    expect(result.error.message).toContain('bad type')
  })

  test('RangeError propagates name', async () => {
    const result = await runtime.run({ code: 'throw new RangeError("out of range")' })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.name).toBe('RangeError')
  })

  test('custom error name propagates', async () => {
    const result = await runtime.run({
      code: 'const e = new Error("suspend"); e.name = "WorkflowSuspend"; throw e',
    })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.name).toBe('WorkflowSuspend')
  })

  test('non-Error throw keeps name as Error', async () => {
    const result = await runtime.run({ code: 'throw "oops"' })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.name).toBe('Error')
  })

  test('error fields carry own enumerable properties', async () => {
    const result = await runtime.run({
      code: `
        const e = new Error("suspend")
        e.name = "WorkflowSuspend"
        e.kind = "waitForEvent"
        e.stepId = "approval"
        throw e
      `,
    })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.name).toBe('WorkflowSuspend')
    expect(result.error.fields).toMatchObject({ kind: 'waitForEvent', stepId: 'approval' })
  })

  test('plain Error has no fields (standard properties are non-enumerable)', async () => {
    const result = await runtime.run({ code: 'throw new Error("plain")' })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.fields).toBeUndefined()
  })
})

// ─────────────────────────────────────────────────────────────────────────────
// 7b. Value boundary contract — data, not behavior
//
// Values cross as V8 serialization blobs, so everything the V8 format carries
// arrives as a real instance in BOTH directions: primitives, bigint, string,
// Date, Map, Set, RegExp, Error, ArrayBuffer, every TypedArray, and cycles.
// Behavior still cannot cross: functions, symbols, promises, and WeakMaps
// fail loudly rather than silently corrupting to `{}`.
// ─────────────────────────────────────────────────────────────────────────────

describe('value boundary contract', () => {
  let runtime: Runtime

  beforeAll(async () => {
    runtime = await createRuntime()
  })

  afterAll(async () => {
    await runtime?.dispose()
  })

  test('exported Uint8Array round-trips as a Uint8Array', async () => {
    const result = await runtime.run({
      code: 'export default new Uint8Array([1, 2, 3])',
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports.default).toBeInstanceOf(Uint8Array)
    expect(Array.from(result.exports.default as Uint8Array)).toEqual([1, 2, 3])
  })

  test('Uint8Array survives host → sandbox → host unchanged', async () => {
    const payload = new Uint8Array([0, 127, 255, 42])
    const result = await runtime.run({
      code: `
        const bytes = await getBytes()
        export default { echoed: bytes, sum: bytes.reduce((a, b) => a + b, 0) }
      `,
      globals: { getBytes: async () => payload },
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    const exported = result.exports.default as { echoed: Uint8Array, sum: number }
    expect(Array.from(exported.echoed)).toEqual([0, 127, 255, 42])
    expect(exported.sum).toBe(424)
  })

  test.each([
    ['Date', 'new Date(1700000000000)', (v: unknown) => {
      expect(v).toBeInstanceOf(Date)
      expect((v as Date).getTime()).toBe(1700000000000)
    }],
    ['Map', 'new Map([["a", 1]])', (v: unknown) => {
      expect(v).toBeInstanceOf(Map)
      expect((v as Map<string, number>).get('a')).toBe(1)
    }],
    ['Set', 'new Set([1, 2, 3])', (v: unknown) => {
      expect(v).toBeInstanceOf(Set)
      expect([...(v as Set<number>)]).toEqual([1, 2, 3])
    }],
    ['RegExp', '/abc/g', (v: unknown) => {
      expect(v).toBeInstanceOf(RegExp)
      expect((v as RegExp).source).toBe('abc')
      expect((v as RegExp).flags).toBe('g')
    }],
    ['ArrayBuffer', 'new Uint8Array([7, 8]).buffer', (v: unknown) => {
      expect(v).toBeInstanceOf(ArrayBuffer)
      expect([...new Uint8Array(v as ArrayBuffer)]).toEqual([7, 8])
    }],
    ['Float32Array', 'new Float32Array([1, 2])', (v: unknown) => {
      expect(v).toBeInstanceOf(Float32Array)
      expect([...(v as Float32Array)]).toEqual([1, 2])
    }],
    ['Error', 'new TypeError("boom")', (v: unknown) => {
      expect(v).toBeInstanceOf(TypeError)
      expect((v as Error).message).toBe('boom')
    }],
    ['bigint', '2n ** 70n', (v: unknown) => {
      expect(v).toBe(2n ** 70n)
    }],
  ])('exporting a %s round-trips as a real instance', async (_name, expr, assertValue) => {
    const result = await runtime.run({ code: `export default ${expr}` })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    assertValue(result.exports.default)
  })

  test('a builtin nested inside a plain object round-trips too', async () => {
    const result = await runtime.run({
      code: 'export default { when: new Date(1700000000000) }',
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    const exported = result.exports.default as { when: Date }
    expect(exported.when).toBeInstanceOf(Date)
    expect(exported.when.getTime()).toBe(1700000000000)
  })

  test('a cyclic export round-trips with its cycle intact', async () => {
    const result = await runtime.run({
      code: 'const o = { x: 1 }; o.self = o; export default o',
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    const exported = result.exports.default as { x: number, self: unknown }
    expect(exported.x).toBe(1)
    expect(exported.self).toBe(exported)
  })

  test('host handler returning a Date delivers a real Date to the sandbox', async () => {
    const result = await runtime.run({
      code: 'const d = await now(); export default { isDate: d instanceof Date, t: d.getTime() }',
      globals: { now: async () => new Date(1700000000000) },
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports.default).toEqual({ isDate: true, t: 1700000000000 })
  })

  test('host handler returning a class instance flattens to its own properties', async () => {
    // Accepted trade-off: Node's serializer has no hook to reject class
    // instances (workerd's `treatClassInstancesAsPlainObjects = false` is not
    // available), so an instance arrives as a plain object of its own
    // enumerable properties. Documented in docs/protocol.md §4.2.
    class Row {
      value = 1
      label = 'x'
      describe(): string {
        return this.label
      }
    }
    const result = await runtime.run({
      code: `
        const r = await fetchRow()
        export default { keys: Object.keys(r).sort(), value: r.value, hasMethod: typeof r.describe }
      `,
      globals: { fetchRow: async () => new Row() },
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports.default).toEqual({
      keys: ['label', 'value'],
      value: 1,
      hasMethod: 'undefined',
    })
  })

  // ── Full value matrix, both directions ──────────────────────────────────
  //
  // The three tests below exist because "it round-trips" is easy to believe
  // and hard to verify: a `Date` that silently flattened to a plain object
  // still prints plausibly, `JSON.stringify` renders a real `Date` as a
  // string and a real `Map` as `{}`, and `toEqual` is structural enough to
  // pass on a flattened value. So they check the **internal slot brand**
  // (`Object.prototype.toString`, which a plain object cannot fake) plus
  // `instanceof` against the *receiving realm's* constructor, on both sides.

  /**
   * Source for a sandbox-side `brand()` — `[object Date]`-style tag plus proof
   * the value is an instance of the sandbox realm's own constructor.
   *
   * A flattened value tags as `Object`; a value carrying another realm's
   * prototype tags correctly but fails `instanceof`. Either way the returned
   * string stops matching, so the test fails loudly instead of passing on a
   * lookalike.
   */
  const BRAND_FN_SRC = `
    const brand = (v) => {
      if (v === null) return 'null'
      if (typeof v !== 'object') return typeof v
      const tag = Object.prototype.toString.call(v).slice(8, -1)
      const ctor = globalThis[tag]
      if (typeof ctor !== 'function' || !(v instanceof ctor)) return tag + '!NOT-AN-INSTANCE'
      return tag === 'Error' ? 'Error:' + v.name : tag
    }
  `

  /**
   * Host-side twin of `BRAND_FN_SRC`, run against what comes back.
   * @param v the value to brand
   */
  function brand(v: unknown): string {
    if (v === null)
      return 'null'
    if (typeof v !== 'object')
      return typeof v
    const tag = Object.prototype.toString.call(v).slice(8, -1)
    const ctor = (globalThis as Record<string, unknown>)[tag]
    if (typeof ctor !== 'function' || !(v instanceof (ctor as new () => object)))
      return `${tag}!NOT-AN-INSTANCE`
    return tag === 'Error' ? `Error:${(v as Error).name}` : tag
  }

  /**
   * One row of the value matrix: a host value, the brand both realms must
   * agree on, and a check that the value still *behaves* like its type after
   * the round trip (a prototype alone proves less than a working method).
   */
  interface ValueCase {
    name: string
    value: HostExportData
    brand: string
    /**
     * Expression evaluated in the sandbox against `v`; must equal `use`.
     */
    useExpr: string
    use: unknown
    assertHost: (v: unknown) => void
  }

  const VALUE_MATRIX: readonly ValueCase[] = [
    {
      name: 'Date',
      value: new Date(1700000000000),
      brand: 'Date',
      useExpr: 'v.getTime()',
      use: 1700000000000,
      assertHost: (v) => {
        expect(v).toBeInstanceOf(Date)
        expect((v as Date).getTime()).toBe(1700000000000)
      },
    },
    {
      name: 'RegExp',
      value: /ab+c/gi,
      brand: 'RegExp',
      useExpr: `[v.source, v.flags, v.test('xABBBCx')].join('|')`,
      use: 'ab+c|gi|true',
      assertHost: (v) => {
        expect(v).toBeInstanceOf(RegExp)
        expect((v as RegExp).source).toBe('ab+c')
        expect((v as RegExp).flags).toBe('gi')
        expect((v as RegExp).test('xABBBCx')).toBe(true)
      },
    },
    {
      name: 'Error',
      value: new TypeError('boom'),
      brand: 'Error:TypeError',
      useExpr: `[v.name, v.message, v instanceof TypeError].join('|')`,
      use: 'TypeError|boom|true',
      assertHost: (v) => {
        expect(v).toBeInstanceOf(TypeError)
        expect((v as Error).message).toBe('boom')
      },
    },
    {
      name: 'Map',
      value: new Map<HostExportData, HostExportData>([['a', 1], [2, 'b']]),
      brand: 'Map',
      useExpr: `[v.size, v.get('a'), v.get(2)].join('|')`,
      use: '2|1|b',
      assertHost: (v) => {
        expect(v).toBeInstanceOf(Map)
        expect([...(v as Map<unknown, unknown>)]).toEqual([['a', 1], [2, 'b']])
      },
    },
    {
      name: 'Set',
      value: new Set<HostExportData>(['x', 2]),
      brand: 'Set',
      useExpr: `[v.size, v.has('x'), v.has(2)].join('|')`,
      use: '2|true|true',
      assertHost: (v) => {
        expect(v).toBeInstanceOf(Set)
        expect([...(v as Set<unknown>)]).toEqual(['x', 2])
      },
    },
    {
      name: 'ArrayBuffer',
      value: new Uint8Array([7, 8, 9]).buffer,
      brand: 'ArrayBuffer',
      useExpr: `[v.byteLength, new Uint8Array(v)[1]].join('|')`,
      use: '3|8',
      assertHost: (v) => {
        expect(v).toBeInstanceOf(ArrayBuffer)
        expect([...new Uint8Array(v as ArrayBuffer)]).toEqual([7, 8, 9])
      },
    },
    {
      name: 'DataView',
      value: new DataView(new Uint8Array([0, 0, 1, 2]).buffer),
      brand: 'DataView',
      useExpr: `[v.byteLength, v.getUint8(3)].join('|')`,
      use: '4|2',
      assertHost: (v) => {
        expect(v).toBeInstanceOf(DataView)
        expect((v as DataView).getUint8(3)).toBe(2)
      },
    },
    {
      name: 'Uint8Array',
      value: new Uint8Array([1, 2, 3]),
      brand: 'Uint8Array',
      useExpr: `[v.length, v[2]].join('|')`,
      use: '3|3',
      assertHost: (v) => {
        expect(v).toBeInstanceOf(Uint8Array)
        expect([...(v as Uint8Array)]).toEqual([1, 2, 3])
      },
    },
    {
      name: 'Float64Array',
      value: new Float64Array([1.5, -2.5]),
      brand: 'Float64Array',
      useExpr: `[v.length, v[1]].join('|')`,
      use: '2|-2.5',
      assertHost: (v) => {
        expect(v).toBeInstanceOf(Float64Array)
        expect([...(v as Float64Array)]).toEqual([1.5, -2.5])
      },
    },
    {
      name: 'BigInt64Array',
      value: new BigInt64Array([-1n, 2n ** 40n]),
      brand: 'BigInt64Array',
      useExpr: `[v.length, v[1].toString()].join('|')`,
      use: `2|${2n ** 40n}`,
      assertHost: (v) => {
        expect(v).toBeInstanceOf(BigInt64Array)
        expect([...(v as BigInt64Array)]).toEqual([-1n, 2n ** 40n])
      },
    },
    {
      name: 'a subarray window',
      value: new Uint8Array([0, 1, 2, 3, 4]).subarray(1, 4),
      brand: 'Uint8Array',
      useExpr: `[v.length, v[0]].join('|')`,
      use: '3|1',
      assertHost: (v) => {
        expect([...(v as Uint8Array)]).toEqual([1, 2, 3])
        expect((v as Uint8Array).byteLength).toBe(3)
      },
    },
    {
      name: 'bigint',
      value: 2n ** 70n,
      brand: 'bigint',
      useExpr: `(v + 1n).toString()`,
      use: `${2n ** 70n + 1n}`,
      assertHost: (v) => {
        expect(v).toBe(2n ** 70n)
      },
    },
    {
      name: 'a plain object',
      value: { a: 1, nested: { b: [1, 2] } },
      brand: 'Object',
      useExpr: `[v.a, v.nested.b[1]].join('|')`,
      use: '1|2',
      assertHost: (v) => {
        expect(v).toEqual({ a: 1, nested: { b: [1, 2] } })
      },
    },
    {
      name: 'an array',
      value: [1, 'two', true, null],
      brand: 'Array',
      useExpr: `v.join(',')`,
      use: '1,two,true,',
      assertHost: (v) => {
        expect(v).toEqual([1, 'two', true, null])
      },
    },
    {
      name: 'null',
      value: null,
      brand: 'null',
      useExpr: `String(v)`,
      use: 'null',
      assertHost: (v) => {
        expect(v).toBeNull()
      },
    },
    {
      name: 'undefined',
      value: undefined,
      brand: 'undefined',
      useExpr: `String(v)`,
      use: 'undefined',
      assertHost: (v) => {
        expect(v).toBeUndefined()
      },
    },
  ]

  test('the whole value matrix survives host → sandbox → host in one array', async () => {
    // One array carrying every type the boundary claims to support, in as a
    // data global and straight back out as the default export. The sandbox
    // brands each element before echoing, so a failure says which element
    // stopped being itself and on which side.
    const result = await runtime.run({
      code: `
        ${BRAND_FN_SRC}
        export default { brands: VALUES.map(brand), echo: VALUES }
      `,
      globals: { VALUES: { kind: 'data', value: VALUE_MATRIX.map((c) => c.value) } },
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    const { brands, echo } = result.exports.default as { brands: string[], echo: unknown[] }
    const expected = VALUE_MATRIX.map((c) => c.brand)

    // Inbound: what the sandbox saw.
    expect(brands).toEqual(expected)
    // Outbound: what came back.
    expect(echo.map(brand)).toEqual(expected)
    expect(echo).toHaveLength(VALUE_MATRIX.length)
    for (const [i, c] of VALUE_MATRIX.entries())
      c.assertHost(echo[i])
  })

  test.each(VALUE_MATRIX.map((c) => [c.name, c] as const))(
    'sandbox receives %s as a working instance and hands it back intact',
    async (_name, c) => {
      // Per-type twin of the array test: narrower blast radius on failure, and
      // it additionally *uses* the value inside the sandbox — a working
      // `getTime()` / `.test()` / `.get()` proves the internal slots crossed,
      // not merely a prototype.
      const result = await runtime.run({
        code: `
          ${BRAND_FN_SRC}
          const v = VALUE
          export default { brand: brand(v), used: ${c.useExpr}, echo: v }
        `,
        globals: { VALUE: { kind: 'data', value: c.value } },
      })
      expect(result.ok).toBe(true)
      if (!result.ok)
        return
      const out = result.exports.default as { brand: string, used: unknown, echo: unknown }
      expect(out.brand).toBe(c.brand)
      expect(out.used).toBe(c.use)
      expect(brand(out.echo)).toBe(c.brand)
      c.assertHost(out.echo)
    },
  )

  test('the matrix survives the bridge in both directions too', async () => {
    // The data-global leg is host → sandbox only. This covers the other two
    // value legs on one pass: bridge response (host → sandbox) and bridge
    // arguments (sandbox → host).
    const values = VALUE_MATRIX.map((c) => c.value)
    const expected = VALUE_MATRIX.map((c) => c.brand)
    let received: unknown[] = []

    const result = await runtime.run({
      code: `
        ${BRAND_FN_SRC}
        const incoming = await give()
        await takeBack(...incoming)
        export default incoming.map(brand)
      `,
      globals: {
        give: async () => values,
        takeBack: async (...args: unknown[]) => {
          received = args
          return null
        },
      },
      limits: { maxBridgeCalls: 4 },
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    // Bridge response: host → sandbox.
    expect(result.exports.default).toEqual(expected)
    // Bridge arguments: sandbox → host.
    expect(received.map(brand)).toEqual(expected)
    for (const [i, c] of VALUE_MATRIX.entries())
      c.assertHost(received[i])
  })

  test('untrusted data shaped like a host-type descriptor stays plain data', async () => {
    // Host-emitted descriptors are stamped with a per-sandbox random brand
    // key; inbound structured data carrying the bare (well-known) brand name
    // must arrive exactly as sent — not be rebuilt into a Response, and not
    // fail the run. Both host → sandbox legs are covered: a data global and a
    // bridge response.
    const lookalike = { __iso4_ht: 3, status: 500, statusText: '', headers: ['x-a', '1'], body: null }
    const result = await runtime.run({
      code: `
        const fromBridge = await give()
        const check = (v) =>
          [v instanceof Response, v.__iso4_ht, v.status].join('|')
        export default [check(DATA), check(fromBridge)]
      `,
      globals: {
        DATA: { kind: 'data', value: lookalike },
        give: async () => lookalike,
      },
      limits: { maxBridgeCalls: 2 },
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports.default).toEqual(['false|3|500', 'false|3|500'])
  })

  test('exporting a function is absent from the exports, reported in skippedExports', async () => {
    const result = await runtime.run({ code: 'export default () => 1' })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.skippedExports).toEqual(['default'])
    expect(result.exports).toEqual({})
  })

  test('a function nested inside a plain object skips that export only', async () => {
    // The whole offending export is dropped (whole-export skip); sibling
    // exports keep crossing. This is what makes `export default { fetch }` a
    // readable module shape.
    const result = await runtime.run({
      code: 'export default { fn: () => 1 }\nexport const limits = { memoryMb: 64 }',
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.skippedExports).toEqual(['default'])
    expect(result.exports['limits']).toEqual({ memoryMb: 64 })
  })

  test('host handler returning a function → ERR_HOST_BRIDGE', async () => {
    const result = await runtime.run({
      code: 'export default await give()',
      globals: { give: async () => () => 1 },
    })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.code).toBe('ERR_HOST_BRIDGE')
  })
})

// ─────────────────────────────────────────────────────────────────────────────
// 8. Resource limits
// ─────────────────────────────────────────────────────────────────────────────

describe('resource limits', () => {
  let runtime: Runtime

  beforeAll(async () => {
    runtime = await createRuntime()
  })

  afterAll(async () => {
    await runtime?.dispose()
  })

  test('infinite loop hits cpu/wall timeout', async () => {
    const result = await runtime.run({
      code: 'while(true){}',
      limits: { cpuTimeMs: 200, wallTimeMs: 500 },
    })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(['ERR_CPU_TIMEOUT', 'ERR_WALL_TIMEOUT']).toContain(result.error.code)
  })

  test('memory limit is enforced — TypedArray', async () => {
    // The heap cap is per-Runtime (uniform across isolates, set at
    // createSandbox) — a dedicated small-cap sandbox exercises it.
    const small = await createRuntime({ maxIsolates: 1, memoryMb: 32 })
    try {
      const result = await small.run({
        code: `
          const arrays = []
          while (true) { arrays.push(new Uint8Array(1024 * 1024)) }
        `,
        limits: { wallTimeMs: 10_000, cpuTimeMs: 10_000 },
      })
      expect(result.ok).toBe(false)
      if (result.ok)
        return
      expect(result.error.code).toBe('ERR_MEMORY_LIMIT')
    } finally {
      await small.dispose()
    }
  }, 15_000)

  test('an error too large to send does not retire the pool slot', async () => {
    // maxExportBytes bounds the success path; nothing bounded a thrown error's
    // message and stack, which are copied verbatim out of the isolate. This
    // built a Result frame of ~140 MB, past the 64 MiB frame ceiling, so the
    // runtime could not write it, treated that as fatal, and closed the socket
    // with no frame at all. The host recycled the dead connection, so at
    // maxIsolates: 1 the sandbox was finished — one line of guest code.
    const small = await createRuntime({ maxIsolates: 1 })
    try {
      const result = await small.run({
        code: `throw new Error('A'.repeat(70e6))`,
        limits: { wallTimeMs: 20_000, cpuTimeMs: 20_000 },
      })
      expect(result.ok).toBe(false)
      if (result.ok)
        return
      // The real error survives, clamped — not swapped for a generic failure.
      expect(result.error.code).toBe('ERR_USER_CODE')
      expect(result.error.message).toMatch(/truncated by iso4: \d+ bytes total/)

      // The point of the test: the one connection is still alive.
      const next = await small.run({ code: 'export default 1 + 1' })
      expect(next.ok).toBe(true)
    } finally {
      await small.dispose()
    }
  }, 30_000)

  test('memory limit is enforced — logs emitted before OOM are preserved', async () => {
    const small = await createRuntime({ maxIsolates: 1, memoryMb: 32 })
    try {
      const result = await small.run({
        code: `
          console.log('before oom')
          const arrays = []
          while (true) { arrays.push(new Uint8Array(1024 * 1024)) }
        `,
        limits: { wallTimeMs: 10_000, cpuTimeMs: 10_000 },
      })
      expect(result.ok).toBe(false)
      if (result.ok)
        return
      expect(result.error.code).toBe('ERR_MEMORY_LIMIT')
      expect(result.stdout.some((l: string) => l.includes('before oom'))).toBe(true)
    } finally {
      await small.dispose()
    }
  }, 15_000)

  test('per-run memoryMb is rejected loudly', async () => {
    // Removed deliberately: the cap is baked into each isolate at creation and
    // prefix runs reuse warm isolates, so a per-run value is impossible.
    await expect(
      runtime.run({
        code: 'export default 1',
        limits: { memoryMb: 32 } as never,
      }),
    ).rejects.toThrow(/memoryMb was removed/)
  })

  test('wall guard fires before cpu guard (tight loop)', async () => {
    // Pure async hang (await neverResolvingPromise) requires Phase 4 bridge.
    // Instead: tight loop with wallTimeMs << cpuTimeMs so wall fires first.
    const result = await runtime.run({
      code: 'let i = 0; while (true) { i++; }',
      limits: { wallTimeMs: 200, cpuTimeMs: 30_000 },
    })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.code).toBe('ERR_WALL_TIMEOUT')
  }, 5_000)

  test('a run within limits completes successfully', async () => {
    const result = await runtime.run({
      code: 'export default 1 + 1',
      limits: { cpuTimeMs: 5000, wallTimeMs: 10000 },
    })
    expect(result.ok).toBe(true)
  })
})

// ─────────────────────────────────────────────────────────────────────────────
// 9. AbortSignal — host-side cancellation
// ─────────────────────────────────────────────────────────────────────────────

describe('AbortSignal cancellation', () => {
  let runtime: Runtime

  beforeAll(async () => {
    runtime = await createRuntime()
  })

  afterAll(async () => {
    await runtime?.dispose()
  })

  test('pre-aborted signal produces ERR_ABORTED immediately', async () => {
    const controller = new AbortController()
    controller.abort()
    const result = await runtime.run({
      code: 'export default 1',
      signal: controller.signal,
    })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    // status discriminant and legacy error code both identify the abort.
    expect(result.status).toBe('aborted')
    expect(result.error.code).toBe('ERR_ABORTED')
  })

  test('aborted result has status "aborted" and is not a "failed"', async () => {
    const controller = new AbortController()
    setTimeout(() => controller.abort(), 50)
    const result = await runtime.run({
      code: `
        import { slowCall } from 'host:slow'
        export default await slowCall()
      `,
      signal: controller.signal,
      imports: {
        'host:slow': {
          slowCall: () =>
            new Promise((resolve) => {
              setTimeout(resolve, 10_000)
            }),
        },
      },
    })
    expect(result.status).toBe('aborted')
    // A deliberate abort must be distinguishable from a genuine failure.
    expect(result.status).not.toBe('failed')
  })

  test('abort reason is surfaced on the result (pre-aborted and in-flight)', async () => {
    // Pre-aborted with an explicit reason.
    const c1 = new AbortController()
    const reason1 = { kind: 'suspend', stepId: 'approval' }
    c1.abort(reason1)
    const pre = await runtime.run({ code: 'export default 1', signal: c1.signal })
    expect(pre.status).toBe('aborted')
    if (pre.status === 'aborted')
      expect(pre.reason).toEqual(reason1)

    // Aborted mid-flight with an explicit reason.
    const c2 = new AbortController()
    const reason2 = { kind: 'suspend', stepId: 'wait-2' }
    setTimeout(() => c2.abort(reason2), 50)
    const mid = await runtime.run({
      code: `
        import { slowCall } from 'host:slow'
        export default await slowCall()
      `,
      signal: c2.signal,
      imports: {
        'host:slow': {
          slowCall: () =>
            new Promise((resolve) => {
              setTimeout(resolve, 10_000)
            }),
        },
      },
    })
    expect(mid.status).toBe('aborted')
    if (mid.status === 'aborted')
      expect(mid.reason).toEqual(reason2)
  })

  test('completed and failed runs carry the matching status', async () => {
    const okResult = await runtime.run({ code: 'export default 1 + 1' })
    expect(okResult.status).toBe('completed')
    expect(okResult.ok).toBe(true)

    const failResult = await runtime.run({ code: 'throw new Error("boom")' })
    expect(failResult.status).toBe('failed')
    expect(failResult.ok).toBe(false)
    if (!failResult.ok && failResult.status === 'failed')
      expect(failResult.error.code).toBe('ERR_USER_CODE')
  })

  test('signal aborted during async execution produces ERR_ABORTED', async () => {
    const controller = new AbortController()
    // abort after 100ms; the run waits on a slow bridge call
    setTimeout(() => controller.abort(), 100)
    const start = Date.now()
    const result = await runtime.run({
      code: `
        import { slowCall } from 'host:slow'
        export default await slowCall()
      `,
      signal: controller.signal,
      imports: {

        'host:slow': {
          slowCall: () =>
            new Promise((resolve) => {
              setTimeout(resolve, 10_000)
            }),
        },
      },
    })
    // Resolves promptly on abort — not after the 10s bridge call or wallTimeMs.
    expect(Date.now() - start).toBeLessThan(2_000)
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.code).toBe('ERR_ABORTED')
  })

  test('graceful abort reports runtime telemetry (bridge records + timings)', async () => {
    // Aborting while the sandbox is suspended awaiting a bridge call takes the
    // graceful terminate path: Rust sends a real ERR_ABORTED result, so
    // the aborted RunResult carries the in-flight bridge record and non-zero
    // timings rather than the synthesized zeros of a socket teardown.
    const controller = new AbortController()
    setTimeout(() => controller.abort(), 100)
    const result = await runtime.run({
      code: `
        import { slowCall } from 'host:slow'
        export default await slowCall()
      `,
      signal: controller.signal,
      imports: {
        'host:slow': {
          slowCall: () =>
            new Promise((resolve) => {
              setTimeout(resolve, 10_000)
            }),
        },
      },
    })
    expect(result.status).toBe('aborted')
    if (result.status !== 'aborted')
      return
    // The in-flight slowCall is recorded — unsettled, so ok is false.
    expect(result.bridgeCalls.length).toBeGreaterThanOrEqual(1)
    expect(result.bridgeCalls[0]?.name).toContain('slowCall')
    expect(result.bridgeCalls[0]?.ok).toBe(false)
    // Real timings from the runtime, not synthesized zeros.
    expect(result.durationMs).toBeGreaterThan(0)
  })

  test('a late BridgeResponse after abort is not observed by the sandbox', async () => {
    // The bridge handler resolves *after* the abort lands. The run must still
    // resolve ERR_ABORTED and the sandbox must never see the returned value.
    let resolveCall: ((v: unknown) => void) | undefined
    const controller = new AbortController()
    const result = await runtime.run({
      code: `
        import { slowCall } from 'host:slow'
        export default await slowCall()
      `,
      signal: controller.signal,
      imports: {
        'host:slow': {
          slowCall: () =>
            new Promise((resolve) => {
              resolveCall = resolve
              // Abort while this call is in flight, then resolve it late.
              setTimeout(() => controller.abort(), 50)
              setTimeout(() => resolve(123), 150)
            }),
        },
      },
    })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.code).toBe('ERR_ABORTED')
    // Ensure the late resolution has fired; it must have had no effect.
    expect(resolveCall).toBeDefined()
    await new Promise<void>((r) => {
      setTimeout(r, 200)
    })
  })

  test('signal aborted during a CPU-bound loop produces ERR_ABORTED without waiting for wallTimeMs', async () => {
    const controller = new AbortController()
    setTimeout(() => controller.abort(), 100)
    const start = Date.now()
    const result = await runtime.run({
      // No bridge calls — a pure CPU spin. wallTimeMs is large so the only
      // prompt exit is via the abort.
      code: 'while (true) {}',
      limits: { cpuTimeMs: 30_000, wallTimeMs: 30_000 },
      signal: controller.signal,
    })
    expect(Date.now() - start).toBeLessThan(2_000)
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.code).toBe('ERR_ABORTED')
  }, 10_000)

  test('signal aborted on one run does not affect a subsequent run', async () => {
    const controller = new AbortController()
    controller.abort()
    await runtime.run({ code: 'export default 1', signal: controller.signal })

    const result = await runtime.run({ code: 'export default 42' })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports.default).toBe(42)
  })

  test('in-flight abort leaves the pool healthy for subsequent runs', async () => {
    // Abort a mid-flight run (torn-down connection), then confirm the pool
    // replaced the slot and later runs still succeed.
    const controller = new AbortController()
    setTimeout(() => controller.abort(), 50)
    const aborted = await runtime.run({
      code: `
        import { slowCall } from 'host:slow'
        export default await slowCall()
      `,
      signal: controller.signal,
      imports: {
        'host:slow': {
          slowCall: () =>
            new Promise((resolve) => {
              setTimeout(resolve, 10_000)
            }),
        },
      },
    })
    expect(aborted.ok).toBe(false)

    // Run enough follow-up work to exercise every pool slot, including the
    // freshly reconnected one.
    const results = await Promise.all(
      Array.from({ length: 8 }, (_, i) =>
        runtime.run({ code: `export default ${i}` })),
    )
    for (const [i, r] of results.entries()) {
      expect(r.ok).toBe(true)
      if (r.ok)
        expect(r.exports.default).toBe(i)
    }
  }, 15_000)

  test('single-isolate pool: a run after an in-flight abort still works (slot is reconnected)', async () => {
    // maxIsolates: 1 means there is exactly ONE connection. If the aborted
    // connection were not replaced, the pool would be permanently empty and
    // this second run could never acquire a slot. Success here proves the
    // dead slot was torn down and a fresh one reconnected in its place.
    const solo = await createRuntime({ maxIsolates: 1 })
    try {
      const controller = new AbortController()
      setTimeout(() => controller.abort(), 50)
      const aborted = await solo.run({
        code: `
          import { slowCall } from 'host:slow'
          export default await slowCall()
        `,
        signal: controller.signal,
        imports: {
          'host:slow': {
            slowCall: () =>
              new Promise((resolve) => {
                setTimeout(resolve, 10_000)
              }),
          },
        },
      })
      expect(aborted.ok).toBe(false)
      if (!aborted.ok)
        expect(aborted.error.code).toBe('ERR_ABORTED')

      // The only slot must have been reconnected — otherwise this hangs until
      // the test times out.
      const result = await solo.run({ code: 'export default 42' })
      expect(result.ok).toBe(true)
      if (result.ok)
        expect(result.exports.default).toBe(42)

      // And it keeps working for more than one follow-up run.
      const again = await solo.run({ code: 'export default 43' })
      expect(again.ok).toBe(true)
      if (again.ok)
        expect(again.exports.default).toBe(43)
    } finally {
      await solo.dispose()
    }
  }, 15_000)

  test('abort triggered synchronously from within a bridge handler (durable-isolates pattern)', async () => {
    // The motivating consumer: a host bridge handler decides to stop the run
    // and calls controller.abort() itself. The run must resolve ERR_ABORTED
    // and the value the handler goes on to return must never reach the sandbox.
    const controller = new AbortController()
    let sandboxSawValue = false
    const result = await runtime.run({
      code: `
        const v = await suspend()
        // Must be unreachable — the run is torn down before this resolves.
        markObserved(v)
        export default v
      `,
      signal: controller.signal,
      globals: {
        suspend: () => {
          // Abort from inside the handler, then resolve late.
          controller.abort()
          return new Promise((resolve) => {
            setTimeout(() => resolve('leaked'), 50)
          })
        },
        markObserved: () => {
          sandboxSawValue = true
        },
      },
    })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.code).toBe('ERR_ABORTED')
    // Give the late resolution time to (wrongly) fire.
    await new Promise<void>((r) => {
      setTimeout(r, 150)
    })
    expect(sandboxSawValue).toBe(false)
  })

  test('prefix.run honors an in-flight abort and keeps the prefix usable', async () => {
    const prefix = await runtime.precompile({
      code: '',
      globals: { slow: () => Promise.resolve('unused') },
    })

    const controller = new AbortController()
    setTimeout(() => controller.abort(), 50)
    const aborted = await prefix.run({
      code: `
        export default await slow()
      `,
      signal: controller.signal,
      globals: {
        // Rebind the declared global to one that never resolves in time.
        slow: () =>
          new Promise((resolve) => {
            setTimeout(() => resolve('late'), 10_000)
          }),
      },
    })
    expect(aborted.ok).toBe(false)
    if (!aborted.ok)
      expect(aborted.error.code).toBe('ERR_ABORTED')

    // The prefix (and the pool) survive: a subsequent run on the same prefix
    // still works.
    const ok = await prefix.run({ code: 'export default 7' })
    expect(ok.ok).toBe(true)
    if (ok.ok)
      expect(ok.exports.default).toBe(7)

    await prefix.dispose()
  }, 15_000)

  test('aborting one in-flight run does not disturb a concurrent run', async () => {
    const abortController = new AbortController()

    const abortedPromise = runtime.run({
      code: `
        import { slowCall } from 'host:slow'
        export default await slowCall()
      `,
      signal: abortController.signal,
      imports: {
        'host:slow': {
          slowCall: () =>
            new Promise((resolve) => {
              setTimeout(resolve, 10_000)
            }),
        },
      },
    })

    // A second run that completes normally while the first is mid-flight.
    const healthyPromise = runtime.run({
      code: `
        import { quickCall } from 'host:quick'
        export default await quickCall()
      `,
      imports: {
        'host:quick': {
          quickCall: () =>
            new Promise((resolve) => {
              setTimeout(() => resolve(99), 200)
            }),
        },
      },
    })

    setTimeout(() => abortController.abort(), 50)

    const [aborted, healthy] = await Promise.all([abortedPromise, healthyPromise])
    expect(aborted.ok).toBe(false)
    if (!aborted.ok)
      expect(aborted.error.code).toBe('ERR_ABORTED')
    expect(healthy.ok).toBe(true)
    if (healthy.ok)
      expect(healthy.exports.default).toBe(99)
  }, 15_000)

  test('aborting after the run already completed is a no-op', async () => {
    const controller = new AbortController()
    const result = await runtime.run({
      code: 'export default 1 + 1',
      signal: controller.signal,
    })
    // Run finished successfully; a late abort must not change the result.
    controller.abort()
    expect(result.ok).toBe(true)
    if (result.ok)
      expect(result.exports.default).toBe(2)

    // And the runtime is still usable afterwards.
    const next = await runtime.run({ code: 'export default 5' })
    expect(next.ok).toBe(true)
  })

  test('a fresh signal on a later run is unaffected by an earlier abort', async () => {
    const first = new AbortController()
    first.abort()
    const abortedResult = await runtime.run({ code: 'export default 1', signal: first.signal })
    expect(abortedResult.ok).toBe(false)

    // A brand-new, un-aborted signal must allow the run to complete.
    const second = new AbortController()
    const result = await runtime.run({ code: 'export default 123', signal: second.signal })
    expect(result.ok).toBe(true)
    if (result.ok)
      expect(result.exports.default).toBe(123)
  })
})

// ─────────────────────────────────────────────────────────────────────────────
// 10. Export size limit
// ─────────────────────────────────────────────────────────────────────────────

describe('export size limit', () => {
  let runtime: Runtime

  beforeAll(async () => {
    runtime = await createRuntime()
  })

  afterAll(async () => {
    await runtime?.dispose()
  })

  test('export within size limit succeeds', async () => {
    const result = await runtime.run({
      code: 'export default "small"',
      limits: { maxExportBytes: 1024 } as never, // maxExportBytes not yet in ResourceLimits public type
    })
    expect(result.ok).toBe(true)
  })

  test('export exceeding size limit is ERR_EXPORT_TOO_LARGE', async () => {
    const result = await runtime.run({
      // 32 MB string — way over any reasonable maxExportBytes
      code: 'export default "x".repeat(32 * 1024 * 1024)',
    })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.code).toBe('ERR_EXPORT_TOO_LARGE')
  })
})

// ─────────────────────────────────────────────────────────────────────────────
// 11. stdout / stderr size limits
// ─────────────────────────────────────────────────────────────────────────────

describe('stdout / stderr size limits', () => {
  let runtime: Runtime

  beforeAll(async () => {
    runtime = await createRuntime()
  })

  afterAll(async () => {
    await runtime?.dispose()
  })

  test('stdout within limit is fully captured', async () => {
    const result = await runtime.run({
      code: 'console.log("hello"); export default 1',
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.stdout).toContain('hello')
  })

  test('stdout over limit is truncated not process-crashing', async () => {
    // Writing many megabytes via console.log must not OOM or crash.
    // The runtime caps at maxStdoutBytes and either truncates or errors cleanly.
    const result = await runtime.run({
      code: `
        for (let i = 0; i < 100_000; i++) console.log("a".repeat(100))
        export default 1
      `,
    })
    // Either succeeds with truncated stdout, or fails with a known error code.
    // Must not hang or crash the process.
    if (result.ok) {
      expect(result.stdout.join('\n').length).toBeLessThanOrEqual(2 * 1024 * 1024)
    } else {
      expect(result.error.code).toBe('ERR_INTERNAL')
    }
  })
})

// ─────────────────────────────────────────────────────────────────────────────
// 12. Configured fetch is just a host bridge global/function
// ─────────────────────────────────────────────────────────────────────────────

describe('configured fetch bridge', () => {
  let runtime: Runtime

  beforeAll(async () => {
    runtime = await createRuntime()
  })

  afterAll(async () => {
    await runtime?.dispose()
  })

  test('configured fetch can return a 3xx response without auto-following', async () => {
    const result = await runtime.run({
      code: `
        const res = await fetch("https://example.com/redirect")
        export default res.status
      `,
      globals: {
        fetch: async () => ({
          status: 302,
          headers: { location: 'https://example.com/target' },
          body: null,
        }),
      },
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports.default).toBe(302)
  })

  test('configured fetch handler throwing surfaces as generic host bridge failure', async () => {
    const result = await runtime.run({
      code: 'export default await fetch("https://example.com")',
      globals: {
        fetch: async () => {
          throw new Error('handler blew up')
        },
      },
    })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.code).toBe('ERR_HOST_BRIDGE')
  })
})

// ─────────────────────────────────────────────────────────────────────────────
// 13. Host bridge error propagation
// ─────────────────────────────────────────────────────────────────────────────

describe('host bridge error propagation', () => {
  let runtime: Runtime

  beforeAll(async () => {
    runtime = await createRuntime()
  })

  afterAll(async () => {
    await runtime?.dispose()
  })

  test('host import function throwing is ERR_HOST_BRIDGE', async () => {
    const result = await runtime.run({
      code: `
        import { boom } from 'host:broken'
        export default await boom()
      `,
      imports: {

        'host:broken': {
          boom: () => {
            throw new Error('host function exploded')
          },
        },
      },
    })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.code).toBe('ERR_HOST_BRIDGE')
  })

  test('host import async function rejecting is ERR_HOST_BRIDGE', async () => {
    const result = await runtime.run({
      code: `
        import { fail } from 'host:broken'
        export default await fail()
      `,
      imports: {

        'host:broken': {
          fail: async () => {
            throw new Error('async rejection')
          },
        },
      },
    })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.code).toBe('ERR_HOST_BRIDGE')
  })

  test('host handler error name and message are preserved on the run error', async () => {
    const result = await runtime.run({
      code: 'export default await boom()',
      globals: {
        boom: async () => {
          throw new TypeError('bad type')
        },
      },
    })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.code).toBe('ERR_HOST_BRIDGE')
    expect(result.error.name).toBe('TypeError')
    expect(result.error.message).toBe('bad type')
    // The host stack never crosses the boundary.
    expect(result.error.stack).toBeUndefined()
  })

  test('host handler error own props arrive as error.fields', async () => {
    const result = await runtime.run({
      code: 'export default await boom()',
      globals: {
        boom: async () => {
          throw Object.assign(new Error('with props'), {
            code: 'E_FOO',
            attempt: 2,
            onRetry: () => {}, // non-serializable — dropped
          })
        },
      },
    })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.code).toBe('ERR_HOST_BRIDGE')
    expect(result.error.fields).toEqual({ code: 'E_FOO', attempt: 2 })
  })

  test('host handler error is catchable in the sandbox and preserves identity', async () => {
    const result = await runtime.run({
      code: `
        let out
        try {
          await boom()
          out = 'did not throw'
        } catch (e) {
          out = {
            name: e.name,
            message: e.message,
            code: e.code,
            isTypeError: e instanceof TypeError,
            isError: e instanceof Error,
          }
        }
        export default out
      `,
      globals: {
        boom: async () => {
          throw Object.assign(new TypeError('bad input'), { code: 'E_BAD' })
        },
      },
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports.default).toEqual({
      name: 'TypeError',
      message: 'bad input',
      code: 'E_BAD',
      isTypeError: true,
      isError: true,
    })
  })

  test('custom error names cross the bridge for sandbox catch logic', async () => {
    class WorkflowTimeout extends Error {
      timeoutMs: number
      constructor(message: string, timeoutMs: number) {
        super(message)
        this.name = 'WorkflowTimeout'
        this.timeoutMs = timeoutMs
      }
    }
    const result = await runtime.run({
      code: `
        let out
        try {
          await runStep()
        } catch (e) {
          out = e.name === 'WorkflowTimeout' ? \`timeout after \${e.timeoutMs}ms\` : 'unexpected'
        }
        export default out
      `,
      globals: {
        runStep: async () => {
          throw new WorkflowTimeout('step timed out', 5000)
        },
      },
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports.default).toBe('timeout after 5000ms')
  })

  test('host stack does not leak into the sandbox', async () => {
    const result = await runtime.run({
      code: `
        let out
        try {
          await boom()
        } catch (e) {
          out = String(e.stack ?? '')
        }
        export default out
      `,
      globals: {
        boom: async () => {
          throw new Error('host-side failure')
        },
      },
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    // Whatever stack the sandbox-side Error carries is sandbox-local; the
    // host frames (this test file) must not appear in it.
    expect(result.exports.default).not.toContain('e2e.test')
  })

  test('sandbox continues after catching a host handler error', async () => {
    const result = await runtime.run({
      code: `
        let recovered
        try {
          await flaky()
        } catch {
          recovered = await stable()
        }
        export default recovered
      `,
      globals: {
        flaky: async () => {
          throw new Error('transient failure')
        },
        stable: async () => 'fallback value',
      },
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports.default).toBe('fallback value')
  })
})

// ─────────────────────────────────────────────────────────────────────────────
// 13b. Custom error classes end-to-end — full fidelity in both directions
// ─────────────────────────────────────────────────────────────────────────────
//
// A realistic rich error (an HTTP failure with status, code, reason, nested
// headers, retryable flag) thrown on either side of the bridge, asserting
// exactly which fields arrive on the other side:
//
//   host → sandbox:  name + message + own props re-attached DIRECTLY on the
//                    caught Error (`e.status`, `e.reason`, …); host stack
//                    NEVER crosses (only a sandbox-local stack exists).
//   sandbox → host:  name + message + own props under `error.fields`
//                    (namespaced so nothing collides with `error.code`); the
//                    sandbox stack IS exposed to the host (host is trusted).

/**
 * Rich host-side error, as a host HTTP client library would define it.
 */
class HttpError extends Error {
  status: number
  code: string
  reason: string
  headers: Record<string, string>
  retryable: boolean

  constructor(status: number, code: string, reason: string, retryable: boolean) {
    super(`HTTP ${status}: ${reason}`)
    this.name = 'HttpError'
    this.status = status
    this.code = code
    this.reason = reason
    this.headers = { 'retry-after': '30', 'x-request-id': 'req_123' }
    this.retryable = retryable
  }
}

describe('custom error classes end-to-end', () => {
  let runtime: Runtime

  beforeAll(async () => {
    runtime = await createRuntime()
  })

  afterAll(async () => {
    await runtime?.dispose()
  })

  test('host-thrown HttpError: sandbox catch sees name, message, and direct props', async () => {
    const result = await runtime.run({
      code: `
        let caught
        try {
          await fetchUser(42)
        } catch (e) {
          caught = {
            name: e.name,
            message: e.message,
            status: e.status,
            code: e.code,
            reason: e.reason,
            reasoning: e.reasoning,
            headers: e.headers,
            retryable: e.retryable,
            isError: e instanceof Error,
            stackHasHostFrames: String(e.stack ?? '').includes('e2e.test'),
          }
        }
        export default caught
      `,
      globals: {
        fetchUser: async () => {
          throw Object.assign(
            new HttpError(503, 'E_UPSTREAM', 'upstream unavailable', true),
            { reasoning: 'circuit breaker open' },
          )
        },
      },
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports.default).toEqual({
      name: 'HttpError',
      message: 'HTTP 503: upstream unavailable',
      status: 503,
      code: 'E_UPSTREAM',
      reason: 'upstream unavailable',
      reasoning: 'circuit breaker open',
      headers: { 'retry-after': '30', 'x-request-id': 'req_123' },
      retryable: true,
      isError: true,
      stackHasHostFrames: false,
    })
  })

  test('host-thrown HttpError: sandbox retry logic can branch on direct props', async () => {
    let attempts = 0
    const result = await runtime.run({
      code: `
        let response
        for (let i = 0; i < 3; i++) {
          try {
            response = await fetchUser(42)
            break
          } catch (e) {
            if (e.name !== 'HttpError' || !e.retryable)
              throw e
          }
        }
        export default response
      `,
      globals: {
        fetchUser: async () => {
          attempts += 1
          if (attempts < 3)
            throw new HttpError(503, 'E_UPSTREAM', 'upstream unavailable', true)
          return { id: 42, name: 'jakob' }
        },
      },
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(attempts).toBe(3)
    expect(result.exports.default).toEqual({ id: 42, name: 'jakob' })
  })

  test('host-thrown HttpError uncaught: RunResult error carries name and fields, no stack', async () => {
    const result = await runtime.run({
      code: 'export default await fetchUser(42)',
      globals: {
        fetchUser: async () => {
          throw new HttpError(404, 'E_NOT_FOUND', 'no such user', false)
        },
      },
    })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.code).toBe('ERR_HOST_BRIDGE')
    expect(result.error.name).toBe('HttpError')
    expect(result.error.message).toBe('HTTP 404: no such user')
    expect(result.error.fields).toEqual({
      status: 404,
      code: 'E_NOT_FOUND',
      reason: 'no such user',
      headers: { 'retry-after': '30', 'x-request-id': 'req_123' },
      retryable: false,
    })
    expect(result.error.stack).toBeUndefined()
  })

  test('sandbox-thrown HttpError: RunResult error carries name, fields, and the sandbox stack', async () => {
    const result = await runtime.run({
      code: `
        class HttpError extends Error {
          constructor(status, code, reason, retryable) {
            super(\`HTTP \${status}: \${reason}\`)
            this.name = 'HttpError'
            this.status = status
            this.code = code
            this.reason = reason
            this.headers = { 'retry-after': '30', 'x-request-id': 'req_123' }
            this.retryable = retryable
          }
        }
        throw new HttpError(429, 'E_RATE_LIMIT', 'too many requests', true)
      `,
    })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.code).toBe('ERR_USER_CODE')
    expect(result.error.name).toBe('HttpError')
    expect(result.error.message).toBe('HTTP 429: too many requests')
    expect(result.error.fields).toEqual({
      status: 429,
      code: 'E_RATE_LIMIT',
      reason: 'too many requests',
      headers: { 'retry-after': '30', 'x-request-id': 'req_123' },
      retryable: true,
    })
    // The sandbox stack IS exposed to the host — the host is the trusted side.
    expect(result.error.stack).toContain('HttpError')
  })

  test('round-trip: rethrowing the caught host error keeps ERR_HOST_BRIDGE and all fields', async () => {
    const result = await runtime.run({
      code: `
        try {
          await fetchUser(42)
        } catch (e) {
          throw e
        }
      `,
      globals: {
        fetchUser: async () => {
          throw new HttpError(503, 'E_UPSTREAM', 'upstream unavailable', true)
        },
      },
    })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    // The rethrown object is still the host bridge error, so it keeps its
    // classification — nothing is lost across the full cycle.
    expect(result.error.code).toBe('ERR_HOST_BRIDGE')
    expect(result.error.name).toBe('HttpError')
    expect(result.error.message).toBe('HTTP 503: upstream unavailable')
    expect(result.error.fields).toEqual({
      status: 503,
      code: 'E_UPSTREAM',
      reason: 'upstream unavailable',
      headers: { 'retry-after': '30', 'x-request-id': 'req_123' },
      retryable: true,
    })
  })

  test('fields added by the sandbox before rethrowing survive to the host', async () => {
    const result = await runtime.run({
      code: `
        try {
          await fetchUser(42)
        } catch (e) {
          e.attemptsMade = 3
          throw e
        }
      `,
      globals: {
        fetchUser: async () => {
          throw new HttpError(503, 'E_UPSTREAM', 'upstream unavailable', true)
        },
      },
    })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.code).toBe('ERR_HOST_BRIDGE')
    expect(result.error.fields).toMatchObject({
      code: 'E_UPSTREAM',
      attemptsMade: 3,
    })
  })

  test('round-trip: sandbox rebuilds a fresh error from the caught one (durable-execution pattern)', async () => {
    // The durable-execution pattern: rebuild an equivalent error in-sandbox. Since
    // carried fields are direct own-enumerable props, a plain spread copies
    // them all; the fresh object is user code's own, so ERR_USER_CODE.
    const result = await runtime.run({
      code: `
        try {
          await fetchUser(42)
        } catch (e) {
          throw Object.assign(new Error(e.message), { ...e })
        }
      `,
      globals: {
        fetchUser: async () => {
          throw new HttpError(503, 'E_UPSTREAM', 'upstream unavailable', true)
        },
      },
    })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.code).toBe('ERR_USER_CODE')
    expect(result.error.name).toBe('HttpError')
    expect(result.error.message).toBe('HTTP 503: upstream unavailable')
    expect(result.error.fields).toEqual({
      status: 503,
      code: 'E_UPSTREAM',
      reason: 'upstream unavailable',
      headers: { 'retry-after': '30', 'x-request-id': 'req_123' },
      retryable: true,
    })
  })

  test('sandbox throw of a bare string arrives as a clean Error result', async () => {
    const result = await runtime.run({
      code: 'throw "some string"',
    })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.code).toBe('ERR_USER_CODE')
    expect(result.error.name).toBe('Error')
    expect(result.error.message).toBe('some string')
    // No wrapper-object garbage: neither a literal "undefined" stack nor
    // the string's character indices as fields.
    expect(result.error.stack).toBeUndefined()
    expect(result.error.fields).toBeUndefined()
  })

  test('host throw of a bare string reaches the sandbox as an Error with that message', async () => {
    const result = await runtime.run({
      code: `
        let caught
        try {
          await boom()
        } catch (e) {
          caught = { isError: e instanceof Error, name: e.name, message: e.message }
        }
        export default caught
      `,
      globals: {
        boom: async () => {
          // eslint-disable-next-line no-throw-literal
          throw 'host string'
        },
      },
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports.default).toEqual({
      isError: true,
      name: 'Error',
      message: 'host string',
    })
  })
})

// ─────────────────────────────────────────────────────────────────────────────
// 14. ERR_UNDECLARED_BINDING — prefix.run() with undeclared globals
// ─────────────────────────────────────────────────────────────────────────────

describe('ERR_UNDECLARED_BINDING', () => {
  let runtime: Runtime

  beforeAll(async () => {
    runtime = await createRuntime()
  })

  afterAll(async () => {
    await runtime?.dispose()
  })

  test('passing undeclared global at run time is ERR_UNDECLARED_BINDING', async () => {
    // The prefix does NOT declare fetch at precompile time.
    // Passing fetch at run time must be rejected — adding it would silently
    // mutate the restored snapshot context in an undeclared way.
    const prefix = await runtime.precompile({ code: '' })
    const result = await prefix.run({
      code: 'export default 1',
      globals: { fetch: async () => ({ status: 200, headers: {}, body: null }) },
    })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.code).toBe('ERR_UNDECLARED_BINDING')
    await prefix.dispose()
  })

  test('declared global at precompile time can be rebound at run time', async () => {
    // fetch IS declared at precompile time (even if the stub is never called).
    // Rebinding at run time is allowed.
    const prefix = await runtime.precompile({
      code: '',
      globals: { fetch: async () => ({ status: 200, headers: {}, body: null }) },
    })
    const result = await prefix.run({
      code: 'export default 1',
      globals: { fetch: async () => ({ status: 418, headers: {}, body: null }) },
    })
    expect(result.ok).toBe(true)
    await prefix.dispose()
  })
})

// ─────────────────────────────────────────────────────────────────────────────
// 15. CPU budget excludes async wait time
// ─────────────────────────────────────────────────────────────────────────────

describe('CPU budget vs wall time', () => {
  let runtime: Runtime

  beforeAll(async () => {
    runtime = await createRuntime()
  })

  afterAll(async () => {
    await runtime?.dispose()
  })

  test.skip('time waiting on bridge does not consume CPU budget', async () => {
    // cpuTimeMs is tight (100ms) but the bridge call takes 300ms.
    // The run must succeed because bridge-wait time is excluded from CPU budget.
    const result = await runtime.run({
      code: `
        import { sleep } from 'host:time'
        await sleep(300)
        export default 'done'
      `,
      limits: { cpuTimeMs: 100, wallTimeMs: 5000 },
      imports: {

        'host:time': {
          sleep: (ms: unknown) =>
            new Promise((r) => {
              setTimeout(r, Number(ms))
            }),
        },
      },
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports.default).toBe('done')
  })

  test('tight synchronous loop hits ERR_CPU_TIMEOUT not ERR_WALL_TIMEOUT', async () => {
    // cpuTimeMs is tight; wallTimeMs is generous.
    // A tight loop must hit the CPU budget, not the wall clock.
    const result = await runtime.run({
      code: 'while(true){}',
      limits: { cpuTimeMs: 100, wallTimeMs: 30_000 },
    })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.code).toBe('ERR_CPU_TIMEOUT')
  })

  test('slow async run with no CPU work hits ERR_WALL_TIMEOUT', async () => {
    // wallTimeMs is tight; cpuTimeMs is generous.
    // A tight loop with wall_time_ms < cpu_time_ms should hit the wall clock
    // before the CPU budget fires.
    // Note: setTimeout is not available without bridge (Phase 4). Using a tight
    // loop instead — wall guard fires first because wallTimeMs << cpuTimeMs.
    const result = await runtime.run({
      code: 'let i = 0; while (true) { i++; }',
      limits: { cpuTimeMs: 30_000, wallTimeMs: 200 },
    })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.code).toBe('ERR_WALL_TIMEOUT')
  }, 5_000)
})

// ─────────────────────────────────────────────────────────────────────────────
// 16. maxBridgeCalls limit
// ─────────────────────────────────────────────────────────────────────────────

describe('maxBridgeCalls limit', () => {
  let runtime: Runtime

  beforeAll(async () => {
    runtime = await createRuntime()
  })

  afterAll(async () => {
    await runtime?.dispose()
  })

  test('exceeding maxBridgeCalls returns ERR_BRIDGE_CALL_LIMIT_EXCEEDED', async () => {
    let calls = 0
    const result = await runtime.run({
      code: `
        await myTool()
        await myTool()
        await myTool()
        await myTool()
        export default 'done'
      `,
      limits: { maxBridgeCalls: 3, cpuTimeMs: 5_000, wallTimeMs: 10_000 },
      globals: {
        myTool: () => {
          calls++
          return null
        },
      },
    })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.code).toBe('ERR_BRIDGE_CALL_LIMIT_EXCEEDED')
    // 3 calls reached the host before the 4th was blocked Rust-side
    expect(calls).toBe(3)
  })

  test('limit violation terminates the run even when sandbox code catches it', async () => {
    // Untrusted code swallowing the limit error in try/catch must not be able
    // to keep executing (or complete the run): the violation terminates V8
    // immediately and uncatchably.
    let calls = 0
    const result = await runtime.run({
      code: `
        let n = 0
        for (let i = 0; i < 10; i++) {
          try {
            await myTool()
            n++
          }
          catch {
            // swallowed — must not keep the run alive
          }
        }
        export default n
      `,
      limits: { maxBridgeCalls: 3, cpuTimeMs: 5_000, wallTimeMs: 10_000 },
      globals: {
        myTool: () => {
          calls++
          return null
        },
      },
    })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.code).toBe('ERR_BRIDGE_CALL_LIMIT_EXCEEDED')
    // No attempt past the limit reaches the host.
    expect(calls).toBe(3)
  })

  test('exactly at limit succeeds', async () => {
    let calls = 0
    const result = await runtime.run({
      code: `
        await myTool()
        await myTool()
        await myTool()
        export default 'done'
      `,
      limits: { maxBridgeCalls: 3, cpuTimeMs: 5_000, wallTimeMs: 10_000 },
      globals: {
        myTool: () => {
          calls++
          return null
        },
      },
    })
    expect(result.ok).toBe(true)
    expect(calls).toBe(3)
  })

  test('zero disables the limit', async () => {
    let calls = 0
    const result = await runtime.run({
      code: `
        await myTool()
        await myTool()
        await myTool()
        await myTool()
        await myTool()
        export default 'done'
      `,
      limits: { maxBridgeCalls: 0, cpuTimeMs: 5_000, wallTimeMs: 10_000 },
      globals: {
        myTool: () => {
          calls++
          return null
        },
      },
    })
    expect(result.ok).toBe(true)
    expect(calls).toBe(5)
  })

  test('default limit of 10 applies when limits not set', async () => {
    // When limits is omitted the TS encoder sends maxBridgeCalls=10.
    // 11 calls should fail; 10 should succeed.
    let callsOnFail = 0
    const failResult = await runtime.run({
      code: `${Array.from({ length: 11 }, () => 'await myTool()').join('\n')
      }\nexport default "done"`,
      globals: { myTool: () => {
        callsOnFail++
        return null
      } },
    })
    expect(failResult.ok).toBe(false)
    if (failResult.ok)
      return
    expect(failResult.error.code).toBe('ERR_BRIDGE_CALL_LIMIT_EXCEEDED')
    expect(callsOnFail).toBe(10)

    let callsOnPass = 0
    const passResult = await runtime.run({
      code: `${Array.from({ length: 10 }, () => 'await myTool()').join('\n')
      }\nexport default "done"`,
      globals: { myTool: () => {
        callsOnPass++
        return null
      } },
    })
    expect(passResult.ok).toBe(true)
    expect(callsOnPass).toBe(10)
  })

  test('limit is shared across multiple declared globals', async () => {
    let calls = 0
    const result = await runtime.run({
      code: `
        await toolA()
        await toolA()
        await toolB()
        await toolB()
        export default 'done'
      `,
      limits: { maxBridgeCalls: 3, cpuTimeMs: 5_000, wallTimeMs: 10_000 },
      globals: {
        toolA: () => {
          calls++
          return null
        },
        toolB: () => {
          calls++
          return null
        },
      },
    })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.code).toBe('ERR_BRIDGE_CALL_LIMIT_EXCEEDED')
    expect(calls).toBe(3)
  })

  test('limit applies on prefix.run() too', async () => {
    const prefix = await runtime.precompile({
      code: '',
      globals: { myTool: () => null },
    })

    let calls = 0
    const result = await prefix.run({
      code: `
        await myTool()
        await myTool()
        await myTool()
        await myTool()
        export default 'done'
      `,
      limits: { maxBridgeCalls: 2, cpuTimeMs: 5_000, wallTimeMs: 10_000 },
      globals: { myTool: () => {
        calls++
        return null
      } },
    })

    expect(result.ok).toBe(false)
    if (result.ok) {
      await prefix.dispose()
      return
    }
    expect(result.error.code).toBe('ERR_BRIDGE_CALL_LIMIT_EXCEEDED')
    expect(calls).toBe(2)
    await prefix.dispose()
  })
})

// ─────────────────────────────────────────────────────────────────────────────
// 9. Async context — AsyncLocalStorage via `node:async_hooks`
// ─────────────────────────────────────────────────────────────────────────────

describe('async context — node:async_hooks', () => {
  let runtime: Runtime

  beforeAll(async () => {
    runtime = await createRuntime()
  })

  afterAll(async () => {
    await runtime?.dispose()
  })

  test('AsyncLocalStorage is importable from node:async_hooks', async () => {
    const result = await runtime.run({
      code: /* js */ `
        import { AsyncLocalStorage } from 'node:async_hooks'
        export default typeof AsyncLocalStorage
      `,
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports.default).toBe('function')
  })

  test('store propagates across await points into nested functions', async () => {
    const result = await runtime.run({
      code: /* js */ `
        import { AsyncLocalStorage } from 'node:async_hooks'
        const als = new AsyncLocalStorage()
        async function deep() {
          await Promise.resolve()
          await Promise.resolve()
          return als.getStore()
        }
        export default await als.run('trace-42', async () => {
          await Promise.resolve()
          return deep()
        })
      `,
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports.default).toBe('trace-42')
  })

  test('getStore() is undefined outside any run()', async () => {
    const result = await runtime.run({
      code: /* js */ `
        import { AsyncLocalStorage } from 'node:async_hooks'
        const als = new AsyncLocalStorage()
        export default als.getStore() === undefined
      `,
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports.default).toBe(true)
  })

  test('nested run() builds a durable-workflow breadcrumb key', async () => {
    // step.do(name, fn): each nested scope appends a path segment; the inner
    // scope sees the accumulated key and the outer scope is restored after.
    const result = await runtime.run({
      code: /* js */ `
        import { AsyncLocalStorage } from 'node:async_hooks'
        const keyScope = new AsyncLocalStorage()
        function step(name, body) {
          const parent = keyScope.getStore() ?? ''
          const key = parent ? parent + '/' + name : name
          return keyScope.run(key, body)
        }
        const seen = []
        await step('charge', async () => {
          await step('validate', async () => {
            await Promise.resolve()
            seen.push(keyScope.getStore())
          })
          await step('capture', async () => {
            await Promise.resolve()
            seen.push(keyScope.getStore())
          })
          seen.push(keyScope.getStore())
        })
        export default seen
      `,
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports.default).toEqual([
      'charge/validate',
      'charge/capture',
      'charge',
    ])
  })

  test('concurrent branches keep isolated stores (the module-variable trap)', async () => {
    const result = await runtime.run({
      code: /* js */ `
        import { AsyncLocalStorage } from 'node:async_hooks'
        const als = new AsyncLocalStorage()
        async function branch(label) {
          return als.run(label, async () => {
            await Promise.resolve()
            await Promise.resolve()
            const a = als.getStore()
            await Promise.resolve()
            const b = als.getStore()
            return a + ':' + b
          })
        }
        export default await Promise.all([branch('A'), branch('B'), branch('C')])
      `,
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports.default).toEqual(['A:A', 'B:B', 'C:C'])
  })

  test('works in postfix code against a precompiled prefix', async () => {
    const prefix = await runtime.precompile({
      code: 'globalThis.workflowRoot = "wf"',
    })
    const result = await prefix.run({
      code: /* js */ `
        import { AsyncLocalStorage } from 'node:async_hooks'
        const als = new AsyncLocalStorage()
        export default await als.run(globalThis.workflowRoot, async () => {
          await Promise.resolve()
          return als.getStore()
        })
      `,
    })
    expect(result.ok).toBe(true)
    if (!result.ok) {
      await prefix.dispose()
      return
    }
    expect(result.exports.default).toBe('wf')
    await prefix.dispose()
  })

  // ── WORKS: the durable-workflow shape ──────────────────────────────────────
  // The `step.do(name, fn)` shim lives in a declared IMPORT that the postfix
  // pulls in. Because the prefix code never imports it, the shim (and its
  // module-global `AsyncLocalStorage`) is resolved at RUN time — where
  // `node:async_hooks` is available — not baked into the snapshot.

  const STEP_SHIM_SOURCE = /* js */ `
    import { AsyncLocalStorage } from 'node:async_hooks'
    const keyScope = new AsyncLocalStorage()
    export function step(name, fn) {
      const parent = keyScope.getStore() ?? ''
      return keyScope.run(parent ? parent + '/' + name : name, fn)
    }
    export function currentKey() { return keyScope.getStore() ?? '' }
  `

  test('WORKS: step.do shim provided as a run-time import, used by the postfix', async () => {
    const result = await runtime.run({
      imports: { 'workflow:steps': STEP_SHIM_SOURCE },
      code: /* js */ `
        import { step, currentKey } from 'workflow:steps'
        const seen = []
        await step('charge', async () => {
          await step('validate', async () => {
            await Promise.resolve()
            seen.push(currentKey())      // 'charge/validate'
          })
          await step('refund', async () => {
            await step('validate', async () => {   // same name, different path
              await Promise.resolve()
              seen.push(currentKey())    // 'charge/refund/validate' — no collision
            })
          })
          seen.push(currentKey())        // 'charge'
        })
        export default seen
      `,
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports.default).toEqual([
      'charge/validate',
      'charge/refund/validate',
      'charge',
    ])
  })

  test('WORKS: prefix pre-warms heavy setup; postfix uses BOTH it and the step shim', async () => {
    // The expensive setup (tools/data) is compiled once into the snapshot and
    // restored cheaply per run. The async-context shim is a run-time import.
    // Both are available to the postfix at the same time.
    const prefix = await runtime.precompile({
      code: 'globalThis.tools = { rate: (n) => n * 2 }',
      imports: { 'workflow:steps': STEP_SHIM_SOURCE },
    })
    const result = await prefix.run({
      code: /* js */ `
        import { step, currentKey } from 'workflow:steps'
        export default await step('order', async () => {
          const doubled = globalThis.tools.rate(21)   // prefix-provided (pre-warmed)
          return await step('price', async () => {
            await Promise.resolve()
            return { key: currentKey(), doubled }      // shim-provided (run-time)
          })
        })
      `,
    })
    expect(result.ok).toBe(true)
    if (!result.ok) {
      await prefix.dispose()
      return
    }
    expect(result.exports.default).toEqual({ key: 'order/price', doubled: 42 })
    await prefix.dispose()
  })

  // ── DOES NOT WORK: async context in prefix (snapshot) code ──────────────────
  // The native bindings can't be captured in a V8 startup snapshot, so
  // `node:async_hooks` does not resolve during precompile(). It fails cleanly
  // with ERR_MODULE_NOT_FOUND (a rejected precompile) — not a crash.

  test('DOES NOT WORK: importing node:async_hooks in prefix code', async () => {
    await expect(
      runtime.precompile({
        code: /* js */ `
          import { AsyncLocalStorage } from 'node:async_hooks'
          globalThis.als = new AsyncLocalStorage()
        `,
      }),
    ).rejects.toMatchObject({ code: 'ERR_MODULE_NOT_FOUND' })
  })

  test('DOES NOT WORK: baking the step shim into the prefix (prefix imports it)', async () => {
    // Same reason: if the PREFIX imports the shim, the shim's own
    // `import ... from 'node:async_hooks'` must resolve at snapshot time, which
    // it can't. Keep the shim as a postfix import instead.
    await expect(
      runtime.precompile({
        code: `import { step } from 'workflow:steps'; globalThis.step = step`,
        imports: { 'workflow:steps': STEP_SHIM_SOURCE },
      }),
    ).rejects.toMatchObject({ code: 'ERR_MODULE_NOT_FOUND' })
  })

  // ── BOUNDARY: only run + getStore are implemented ───────────────────────────

  test('BOUNDARY: only run() and getStore() exist (no enterWith/exit)', async () => {
    const result = await runtime.run({
      code: /* js */ `
        import { AsyncLocalStorage } from 'node:async_hooks'
        const als = new AsyncLocalStorage()
        export default {
          run: typeof als.run,
          getStore: typeof als.getStore,
          enterWith: typeof als.enterWith,
          exit: typeof als.exit,
        }
      `,
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports.default).toEqual({
      run: 'function',
      getStore: 'function',
      enterWith: 'undefined',
      exit: 'undefined',
    })
  })
})

// ─────────────────────────────────────────────────────────────────────────────
// 18. Bridge report — result.bridgeCalls + result.cpuTimeMs
// ─────────────────────────────────────────────────────────────────────────────

describe('bridge report', () => {
  let runtime: Runtime

  beforeAll(async () => {
    runtime = await createRuntime()
  })

  afterAll(async () => {
    await runtime?.dispose()
  })

  test('run with no bridge calls reports empty bridgeCalls and a CPU time', async () => {
    const result = await runtime.run({ code: 'export default 1' })
    expect(result.ok).toBe(true)
    expect(result.bridgeCalls).toEqual([])
    expect(result.cpuTimeMs).toBeGreaterThan(0)
    expect(result.cpuTimeMs).toBeLessThanOrEqual(result.durationMs)
  })

  test('plain global calls are recorded in order with metadata', async () => {
    const result = await runtime.run({
      code: `
        await toolA('x'.repeat(500))
        await toolA(2)
        await toolB()
        export default 'done'
      `,
      globals: {
        toolA: () => 'y'.repeat(2000),
        toolB: () => null,
      },
    })
    expect(result.ok).toBe(true)
    expect(result.bridgeCalls).toHaveLength(3)
    expect(result.bridgeCalls.map((c) => c.name)).toEqual(['toolA', 'toolA', 'toolB'])
    const [first] = result.bridgeCalls
    // ~500-char string argument → payload well above 500 bytes.
    expect(first.argBytes).toBeGreaterThan(500)
    // ~2000-char return value → response value above 2000 bytes.
    expect(first.responseBytes).toBeGreaterThan(2000)
    for (const call of result.bridgeCalls) {
      expect(call.ok).toBe(true)
      expect(call.blocked).toBe(false)
      expect(call.startMs).toBeGreaterThanOrEqual(0)
      expect(call.durationMs).toBeGreaterThanOrEqual(0)
      expect(call.argBytes).toBeGreaterThan(0)
      expect(call.responseBytes).toBeGreaterThan(0)
      // Entries live on the run clock, so they fit inside the run duration.
      expect(call.startMs).toBeLessThanOrEqual(result.durationMs)
    }
    // Attempt order matches call order.
    expect(result.bridgeCalls[0].startMs).toBeLessThanOrEqual(result.bridgeCalls[1].startMs)
    expect(result.bridgeCalls[1].startMs).toBeLessThanOrEqual(result.bridgeCalls[2].startMs)
  })

  test('cpuTimeMs excludes time spent waiting on host handlers', async () => {
    const result = await runtime.run({
      code: 'export default await slow()',
      globals: {
        slow: () =>
          new Promise((resolve) => {
            setTimeout(() => resolve('done'), 150)
          }),
      },
    })
    expect(result.ok).toBe(true)
    // The run waited ~150ms on the handler…
    expect(result.durationMs).toBeGreaterThan(100)
    // …but barely executed any JS.
    expect(result.cpuTimeMs).toBeLessThan(result.durationMs / 2)
    // The bridge entry's round-trip covers the wait.
    expect(result.bridgeCalls[0].durationMs).toBeGreaterThan(100)
  })

  test('shimmed global is reported under its public name', async () => {
    const result = await runtime.run({
      code: 'export default await fetchLike(\'u\')',
      globals: {
        fetchLike: {
          kind: 'bridge-with-shim' as const,
          handler: (() => ({ status: 200 })) as (...args: unknown[]) => unknown,
          shim: '(r) => r.status',
        },
      },
    })
    expect(result.ok).toBe(true)
    expect(result.exports.default).toBe(200)
    expect(result.bridgeCalls).toHaveLength(1)
    expect(result.bridgeCalls[0].name).toBe('fetchLike')
  })

  test('host-module import calls are reported as <specifier>.<path>', async () => {
    const result = await runtime.run({
      code: `
        import { query } from 'tools:search'
        export default await query('term')
      `,
      imports: {
        'tools:search': {
          query: (q: unknown) => `result for ${String(q)}`,
        },
      },
    })
    expect(result.ok).toBe(true)
    expect(result.bridgeCalls).toHaveLength(1)
    expect(result.bridgeCalls[0].name).toBe('tools:search.query')
    expect(result.bridgeCalls[0].ok).toBe(true)
  })

  test('handler errors are recorded as ok: false with zero response bytes', async () => {
    const result = await runtime.run({
      code: `
        let failed = false
        try {
          await broken()
        }
        catch {
          failed = true
        }
        export default failed
      `,
      globals: {
        broken: () => {
          throw new Error('boom')
        },
      },
    })
    // Handler errors are catchable in the sandbox — the run itself succeeds.
    expect(result.ok).toBe(true)
    expect(result.exports.default).toBe(true)
    expect(result.bridgeCalls).toHaveLength(1)
    expect(result.bridgeCalls[0].ok).toBe(false)
    expect(result.bridgeCalls[0].blocked).toBe(false)
    expect(result.bridgeCalls[0].responseBytes).toBe(0)
  })

  test('failed runs still carry the bridge report', async () => {
    const result = await runtime.run({
      code: `
        await tool()
        throw new Error('after the call')
      `,
      globals: { tool: () => 1 },
    })
    expect(result.ok).toBe(false)
    expect(result.bridgeCalls).toHaveLength(1)
    expect(result.bridgeCalls[0].name).toBe('tool')
    expect(result.bridgeCalls[0].ok).toBe(true)
  })

  test('attempts blocked by maxBridgeCalls are recorded as blocked entries', async () => {
    const result = await runtime.run({
      code: `
        await tool()
        await tool()
        await tool()
        await tool()
        export default 'done'
      `,
      limits: { maxBridgeCalls: 3, cpuTimeMs: 5_000, wallTimeMs: 10_000 },
      globals: { tool: () => null },
    })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.code).toBe('ERR_BRIDGE_CALL_LIMIT_EXCEEDED')
    // The 4th attempt never reached the host but is on the record — blocked.
    expect(result.bridgeCalls).toHaveLength(4)
    expect(result.bridgeCalls.slice(0, 3).every((c) => c.ok && !c.blocked)).toBe(true)
    const violating = result.bridgeCalls[3]
    expect(violating.blocked).toBe(true)
    expect(violating.ok).toBe(false)
    expect(violating.responseBytes).toBe(0)
  })

  test('aborted runs carry the bridge report from the graceful terminate result', async () => {
    // Graceful termination: the abort lands while the run is suspended
    // awaiting `hang()`, so Rust sends a real ERR_ABORTED result carrying the
    // in-flight bridge record and timings — no longer synthesized zeros.
    const controller = new AbortController()
    setTimeout(() => controller.abort(), 50)
    const result = await runtime.run({
      code: 'export default await hang()',
      signal: controller.signal,
      globals: {
        hang: () =>
          new Promise((resolve) => {
            setTimeout(resolve, 10_000)
          }),
      },
    })
    expect(result.status).toBe('aborted')
    // The in-flight hang() call is recorded — unsettled, so ok is false.
    expect(result.bridgeCalls).toHaveLength(1)
    expect(result.bridgeCalls[0].name).toBe('hang')
    expect(result.bridgeCalls[0].ok).toBe(false)
    expect(result.durationMs).toBeGreaterThan(0)
    expect(result.cpuTimeMs).toBeGreaterThanOrEqual(0)
  })

  test('prefix.run() carries the bridge report too', async () => {
    const prefix = await runtime.precompile({
      code: 'globalThis.ready = true',
      globals: { tool: () => 'v' },
    })
    const result = await prefix.run({ code: 'export default await tool()' })
    expect(result.ok).toBe(true)
    expect(result.exports.default).toBe('v')
    expect(result.bridgeCalls).toHaveLength(1)
    expect(result.bridgeCalls[0].name).toBe('tool')
    expect(result.cpuTimeMs).toBeGreaterThan(0)
    await prefix.dispose()
  })
})

// ─────────────────────────────────────────────────────────────────────────────
// Host → sandbox calls: run({ code, call }) and prefix.call()
// ─────────────────────────────────────────────────────────────────────────────

describe('host → sandbox calls', () => {
  let runtime: Runtime

  beforeAll(async () => {
    runtime = await createRuntime()
  })

  afterAll(async () => {
    await runtime?.dispose()
  })

  test('run({ code, call }) resolves a named export and returns its value', async () => {
    const result = await runtime.run({
      code: 'export function add(a, b) { return a + b }',
      call: { export: 'add', args: [2, 40] },
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.value).toBe(42)
    expect('exports' in result).toBe(false)
  })

  test('default.fetch receives args and the exported object as `this`', async () => {
    const result = await runtime.run({
      code: `export default { tag: 'worker-a', fetch(method) { return this.tag + ':' + method } }`,
      call: { export: 'default.fetch', args: ['POST'] },
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.value).toBe('worker-a:POST')
  })

  test('args default to an empty list', async () => {
    const result = await runtime.run({
      code: 'export function count(...args) { return args.length }',
      call: { export: 'count' },
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.value).toBe(0)
  })

  test('async handlers settle; sync handlers work too', async () => {
    const asyncResult = await runtime.run({
      code: 'export async function f() { return (await 1) + (await 2) }',
      call: { export: 'f' },
    })
    expect(asyncResult.ok && asyncResult.value === 3).toBe(true)
    const syncResult = await runtime.run({
      code: 'export function f() { return "sync" }',
      call: { export: 'f' },
    })
    expect(syncResult.ok && syncResult.value === 'sync').toBe(true)
  })

  test('a real Request crosses in and a real Response crosses back', async () => {
    const prefix = await runtime.prepare({
      code: `export default {
        async fetch(request) {
          const body = await request.text()
          return new Response('echo:' + request.method + ':' + body, { status: 201 })
        },
      }`,
    })
    const result = await prefix.call({
      export: 'default.fetch',
      args: [new Request('https://example.com/in', { method: 'POST', body: 'hi' })],
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    const response = result.value as Response
    expect(response).toBeInstanceOf(Response)
    expect(response.status).toBe(201)
    expect(await response.text()).toBe('echo:POST:hi')
    await prefix.dispose()
  })

  test('prefix.call() reaches module-scope closure state', async () => {
    const prefix = await runtime.prepare({
      code: `let calls = 0
             export function bump() { calls += 1; return calls }`,
    })
    // Each call gets a fresh isolate (one-shot model), so the closure state
    // is per-call — the point is that module scope is reachable at all.
    const result = await prefix.call({ export: 'bump' })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.value).toBe(1)
    await prefix.dispose()
  })

  test('prefix.call() rebinds declared bridge globals per call', async () => {
    const prefix = await runtime.prepare({
      code: 'export async function handler() { return await db() }',
      globals: { db: async () => 'default-db' },
    })
    const withDefault = await prefix.call({ export: 'handler' })
    expect(withDefault.ok && withDefault.value === 'default-db').toBe(true)
    const rebound = await prefix.call({
      export: 'handler',
      globals: { db: async () => 'tenant-db' },
    })
    expect(rebound.ok && rebound.value === 'tenant-db').toBe(true)
    await prefix.dispose()
  })

  test('an async handler makes multiple bridge calls mid-request', async () => {
    const seen: string[] = []
    const result = await runtime.run({
      code: `export default {
        async fetch(id) {
          const a = await lookup(id)
          const b = await lookup(a)
          return 'final:' + b
        },
      }`,
      globals: {
        lookup: async (v: unknown) => {
          seen.push(String(v))
          return `${String(v)}+`
        },
      },
      call: { export: 'default.fetch', args: ['x'] },
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.value).toBe('final:x++')
    expect(seen).toEqual(['x', 'x+'])
    expect(result.bridgeCalls).toHaveLength(2)
  })

  test('unknown export path → ERR_CALL_TARGET_NOT_FOUND', async () => {
    const result = await runtime.run({
      code: 'export default { fetch() {} }',
      call: { export: 'default.missing' },
    })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.code).toBe('ERR_CALL_TARGET_NOT_FOUND')
    expect(result.error.message).toContain('default.missing')
  })

  test('non-callable target → ERR_CALL_TARGET_NOT_FOUND', async () => {
    const result = await runtime.run({
      code: 'export const n = 42',
      call: { export: 'n' },
    })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.code).toBe('ERR_CALL_TARGET_NOT_FOUND')
    expect(result.error.message).toContain('not callable')
  })

  test('a throw inside the handler → ERR_USER_CODE', async () => {
    const result = await runtime.run({
      code: 'export function f() { throw new Error("boom") }',
      call: { export: 'f' },
    })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.code).toBe('ERR_USER_CODE')
    expect(result.error.message).toBe('boom')
  })

  test('a pre-aborted signal resolves to an aborted result', async () => {
    const prefix = await runtime.prepare({ code: 'export function f() { return 1 }' })
    const controller = new AbortController()
    controller.abort('why')
    const result = await prefix.call({ export: 'f', signal: controller.signal })
    expect(result.status).toBe('aborted')
    if (result.status === 'aborted')
      expect(result.reason).toBe('why')
    await prefix.dispose()
  })

  test('logs and telemetry ride along with a call result', async () => {
    const result = await runtime.run({
      code: 'export function f() { console.log("from handler"); return 1 }',
      call: { export: 'f' },
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.stdout).toEqual(['from handler'])
    expect(result.durationMs).toBeGreaterThan(0)
  })
})

// ─────────────────────────────────────────────────────────────────────────────
// Web-server pattern: worker-style fetch handlers driven from the Node side.
// The host passes real Request instances in via `call`, sandbox code behaves
// like an HTTP handler (routing, JSON, binary bodies, an upstream fetch over
// the bridge), and the host gets real Response instances back.
// ─────────────────────────────────────────────────────────────────────────────

describe('web-server pattern (worker-style fetch handlers)', () => {
  let runtime: Runtime

  beforeAll(async () => {
    runtime = await createRuntime()
  })

  afterAll(async () => {
    await runtime?.dispose()
  })

  test('a JSON API handler routes on method and URL and answers Response.json', async () => {
    const prefix = await runtime.prepare({
      code: `export default {
        async fetch(request) {
          const url = new URL(request.url)
          if (request.method !== 'POST' || url.pathname !== '/api/items')
            return new Response('not found', { status: 404 })
          const item = await request.json()
          return Response.json(
            { created: item.name, q: url.searchParams.get('q') },
            { status: 201, headers: { 'x-handler': 'items' } },
          )
        },
      }`,
    })

    const created = await prefix.call({
      export: 'default.fetch',
      args: [new Request('https://api.example.com/api/items?q=7', {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ name: 'widget' }),
      })],
    })
    expect(created.ok).toBe(true)
    if (!created.ok)
      return
    const response = created.value as Response
    expect(response).toBeInstanceOf(Response)
    expect(response.status).toBe(201)
    expect(response.headers.get('content-type')).toBe('application/json')
    expect(response.headers.get('x-handler')).toBe('items')
    expect(await response.json()).toEqual({ created: 'widget', q: '7' })

    const missed = await prefix.call({
      export: 'default.fetch',
      args: [new Request('https://api.example.com/other')],
    })
    expect(missed.ok).toBe(true)
    if (!missed.ok)
      return
    expect((missed.value as Response).status).toBe(404)
    await prefix.dispose()
  })

  test('a binary upload round-trips through the handler byte for byte', async () => {
    const payload = new Uint8Array([0, 1, 2, 253, 254, 255])
    const result = await runtime.run({
      code: `export default {
        async fetch(request) {
          const bytes = new Uint8Array(await request.arrayBuffer())
          bytes.reverse()
          return new Response(bytes, {
            headers: { 'content-type': 'application/octet-stream' },
          })
        },
      }`,
      call: {
        export: 'default.fetch',
        args: [new Request('https://example.com/upload', { method: 'PUT', body: payload })],
      },
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    const response = result.value as Response
    expect(response).toBeInstanceOf(Response)
    const echoed = new Uint8Array(await response.arrayBuffer())
    expect(Array.from(echoed)).toEqual([255, 254, 253, 2, 1, 0])
  })

  test('a handler proxies an upstream fetch over the bridge, cookies intact', async () => {
    // The full loop a proxying worker exercises: host Request in (call args),
    // sandbox Request out to the host handler (bridge args), host Response in
    // (bridge response), sandbox Response out (call result). Duplicate
    // set-cookie entries must survive every leg — a Record cannot carry them,
    // so this pins that no leg folds headers through one.
    let upstreamSaw: Request | undefined
    const result = await runtime.run({
      code: `export default {
        async fetch(request) {
          const upstreamResponse = await upstream(
            new Request('https://origin.internal' + new URL(request.url).pathname, {
              headers: { 'x-forwarded-host': new URL(request.url).host },
            }),
          )
          const headers = new Headers({ 'x-proxied': '1' })
          for (const cookie of upstreamResponse.headers.getSetCookie())
            headers.append('set-cookie', cookie)
          return new Response(await upstreamResponse.text(), {
            status: upstreamResponse.status,
            headers,
          })
        },
      }`,
      globals: {
        upstream: async (request: unknown) => {
          upstreamSaw = request as Request
          return new Response('from origin', {
            status: 203,
            headers: [
              ['set-cookie', 'a=1'],
              ['set-cookie', 'b=2'],
            ],
          })
        },
      },
      call: {
        export: 'default.fetch',
        args: [new Request('https://edge.example.com/assets/logo')],
      },
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    // The bridge handler received a real Request built inside the sandbox.
    expect(upstreamSaw).toBeInstanceOf(Request)
    expect(upstreamSaw?.url).toBe('https://origin.internal/assets/logo')
    expect(upstreamSaw?.headers.get('x-forwarded-host')).toBe('edge.example.com')
    const response = result.value as Response
    expect(response).toBeInstanceOf(Response)
    expect(response.status).toBe(203)
    expect(response.headers.get('x-proxied')).toBe('1')
    expect(response.headers.getSetCookie()).toEqual(['a=1', 'b=2'])
    expect(await response.text()).toBe('from origin')
  })

  test('sequential requests against one prepared handler stay independent', async () => {
    // The warm-instance path: the first call cold-starts an instance, later
    // calls reuse it, and every call's Request must rehydrate on that
    // instance's thread exactly like the first.
    const prefix = await runtime.prepare({
      code: `export default {
        async fetch(request) {
          return Response.json({
            method: request.method,
            path: new URL(request.url).pathname,
            body: request.method === 'GET' ? null : await request.text(),
          })
        },
      }`,
    })
    const requests = [
      new Request('https://example.com/a'),
      new Request('https://example.com/b', { method: 'POST', body: 'two' }),
      new Request('https://example.com/c', { method: 'PUT', body: 'three' }),
    ]
    const seen: unknown[] = []
    for (const request of requests) {
      const result = await prefix.call({ export: 'default.fetch', args: [request] })
      expect(result.ok).toBe(true)
      if (!result.ok)
        return
      seen.push(await (result.value as Response).json())
    }
    expect(seen).toEqual([
      { method: 'GET', path: '/a', body: null },
      { method: 'POST', path: '/b', body: 'two' },
      { method: 'PUT', path: '/c', body: 'three' },
    ])
    await prefix.dispose()
  })
})

// ─────────────────────────────────────────────────────────────────────────────
// Code generation from strings: a prepare()-time capability. Setup code
// may compile fast paths with eval / new Function; run code may only call
// what setup built, and gets a catchable EvalError if it tries to generate
// code of its own.
// ─────────────────────────────────────────────────────────────────────────────

describe('eval and new Function are prepare()-time only', () => {
  let runtime: Runtime

  beforeAll(async () => {
    runtime = await createRuntime()
  })

  afterAll(async () => {
    await runtime?.dispose()
  })

  test('setup-compiled functions work in runs; run code cannot eval', async () => {
    // The zod-shaped pattern: compile a matcher from strings once at
    // prepare(), call it cheaply per run.
    const prefix = await runtime.prepare({
      code: `globalThis.matcher = new Function('s', 'return /^item-[0-9]+$/.test(s)')`,
    })
    const result = await prefix.execute({
      code: `
        const denied = (() => {
          try { eval('1'); return 'allowed' }
          catch (e) { return e instanceof EvalError && e.name }
        })()
        export default { ok: matcher('item-42'), no: matcher('nope'), denied }
      `,
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports.default).toEqual({ ok: true, no: false, denied: 'EvalError' })
    await prefix.dispose()
  })

  test('one-off runs deny eval and new Function from the first line', async () => {
    const result = await runtime.run({
      code: `
        const probe = (fn) => { try { fn(); return 'allowed' } catch (e) { return e.name } }
        export default [probe(() => eval('1')), probe(() => new Function('return 1'))]
      `,
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports.default).toEqual(['EvalError', 'EvalError'])
  })

  test('a global declared enumerable: false is hidden from enumeration but callable', async () => {
    const result = await runtime.run({
      code: `
        const keys = Object.keys(globalThis)
        export default {
          hiddenListed: keys.includes('hiddenTool') || keys.includes('SECRETS'),
          visibleListed: keys.includes('visibleTool'),
          hiddenWorks: await hiddenTool('x'),
          secret: SECRETS.token,
        }
      `,
      globals: {
        hiddenTool: { kind: 'bridge', handler: async (q: unknown) => `ok:${String(q)}`, enumerable: false },
        SECRETS: { kind: 'data', value: { token: 't-1' }, enumerable: false },
        visibleTool: async () => 1,
      },
      limits: { maxBridgeCalls: 2 },
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports.default).toEqual({
      hiddenListed: false,
      visibleListed: true,
      hiddenWorks: 'ok:x',
      secret: 't-1',
    })
  })

  test('runtime internals never surface through enumeration', async () => {
    // A tool global plus a host import (which installs the internal
    // dispatcher). Enumeration sees the declared name only; the plumbing
    // stays out of Object.keys / for-in, matching the web classes.
    const result = await runtime.run({
      code: `
        import { search } from 'tools:web'
        const keys = Object.keys(globalThis)
        const forIn = []
        for (const k in globalThis) forIn.push(k)
        export default {
          internals: [...keys, ...forIn].filter((k) => k.startsWith('__iso4_')),
          tool: keys.includes('myTool'),
          searchWorks: await search('q'),
        }
      `,
      globals: { myTool: async () => 1 },
      imports: { 'tools:web': { search: async (q: unknown) => `hit:${String(q)}` } },
      limits: { maxBridgeCalls: 2 },
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports.default).toEqual({
      internals: [],
      tool: true,
      searchWorks: 'hit:q',
    })
  })
})

// ─────────────────────────────────────────────────────────────────────────────
// waitUntil: the value ships early, registered background work keeps
// running under the grace budget, and the outcome arrives on result.waitUntil.
// ─────────────────────────────────────────────────────────────────────────────

describe('waitUntil', () => {
  let runtime: Runtime

  beforeAll(async () => {
    runtime = await createRuntime()
  })

  afterAll(async () => {
    await runtime?.dispose()
  })

  test('the result resolves before slow background work, which still completes', async () => {
    let auditWritten = false
    let releaseAudit: () => void = () => {}
    const gate = new Promise<void>((resolve) => {
      releaseAudit = resolve
    })

    const result = await runtime.run({
      code: `
        waitUntil((async () => { await audit('record') })())
        export default 'answered'
      `,
      globals: {
        audit: async (entry: unknown) => {
          await gate
          auditWritten = true
          return `stored:${String(entry)}`
        },
      },
      limits: { maxBridgeCalls: 2 },
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    // The caller has the value while the background bridge call is still
    // parked on the host handler.
    expect(result.exports.default).toBe('answered')
    expect(auditWritten).toBe(false)
    expect(result.waitUntil).toBeDefined()

    releaseAudit()
    const report = await result.waitUntil!
    expect(report.status).toBe('settled')
    expect(auditWritten).toBe(true)
  })

  test('a run that registers nothing has no waitUntil field', async () => {
    const result = await runtime.run({ code: 'export default 1' })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.waitUntil).toBeUndefined()
  })

  test('a rejected background promise reports failed; the run stays successful', async () => {
    const result = await runtime.run({
      code: `
        waitUntil(Promise.reject(new TypeError('flush failed')))
        export default 'ok'
      `,
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    const report = await result.waitUntil!
    expect(report.status).toBe('failed')
    expect(report.error).toEqual({ name: 'TypeError', message: 'flush failed' })
  })

  test('unresolvable background work truncates at the grace budget; the slot recovers', async () => {
    const result = await runtime.run({
      code: `
        waitUntil(new Promise(() => {}))
        export default 'ok'
      `,
      limits: { graceMs: 200 },
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    const report = await result.waitUntil!
    expect(report.status).toBe('truncated')

    // The connection went back to the pool after the epilogue; new runs work.
    const next = await runtime.run({ code: 'export default 2' })
    expect(next.ok && next.exports.default === 2).toBe(true)
  })

  test('graceMs: 0 disables the epilogue entirely', async () => {
    const result = await runtime.run({
      code: `
        waitUntil(new Promise(() => {}))
        export default 'ok'
      `,
      limits: { graceMs: 0 },
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.waitUntil).toBeUndefined()
  })

  test('a worker-style handler responds early and finishes its write on a warm instance', async () => {
    const written: string[] = []
    const prefix = await runtime.prepare({
      code: `export default {
        async fetch(request) {
          waitUntil(db(new URL(request.url).pathname))
          return new Response('accepted', { status: 202 })
        },
      }`,
      globals: { db: async (row: unknown) => {
        written.push(String(row))
        return 1
      } },
    })
    for (const path of ['/a', '/b']) {
      const result = await prefix.call({
        export: 'default.fetch',
        args: [new Request(`https://example.com${path}`)],
        globals: { db: async (row: unknown) => {
          written.push(String(row))
          return 1
        } },
      })
      expect(result.ok).toBe(true)
      if (!result.ok)
        return
      expect((result.value as Response).status).toBe(202)
      const report = await result.waitUntil!
      expect(report.status).toBe('settled')
      // The call was ATTEMPTED during the run (waitUntil(db(...)) invokes db
      // synchronously), so its record ships with the run's own telemetry;
      // only its response was consumed during grace.
      expect(result.bridgeCalls.map((c) => c.name)).toEqual(['db'])
      expect(report.bridgeCalls).toEqual([])
    }
    expect(written).toEqual(['/a', '/b'])
    await prefix.dispose()
  })

  test('aborting during the grace phase cancels the background work', async () => {
    const controller = new AbortController()
    const started = Date.now()
    const result = await runtime.run({
      code: `
        waitUntil(new Promise(() => {}))
        export default 'ok'
      `,
      limits: { graceMs: 10_000 },
      signal: controller.signal,
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    controller.abort(new Error('caller moved on'))
    const report = await result.waitUntil!
    expect(report.status).toBe('truncated')
    // Cancelled promptly, not held for the 10 s budget.
    expect(Date.now() - started).toBeLessThan(3_000)

    const next = await runtime.run({ code: 'export default 3' })
    expect(next.ok && next.exports.default === 3).toBe(true)
  })

  test('a huge rejection message is capped and never costs the connection', async () => {
    const result = await runtime.run({
      code: `
        waitUntil(Promise.reject(new Error('x'.repeat(80 * 1024 * 1024))))
        export default 'ok'
      `,
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    const report = await result.waitUntil!
    expect(report.status).toBe('failed')
    // Capped at the runtime, not lost to an oversized frame.
    expect(report.error!.message.length).toBeLessThan(3_000)
    expect(report.error!.message.endsWith('[truncated]')).toBe(true)

    const next = await runtime.run({ code: 'export default 4' })
    expect(next.ok && next.exports.default === 4).toBe(true)
  })

  test('waitUntil is importable from iso4:runtime', async () => {
    const result = await runtime.run({
      code: `
        import { waitUntil as wu } from 'iso4:runtime'
        wu(Promise.resolve())
        export default typeof wu
      `,
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports.default).toBe('function')
    expect((await result.waitUntil!).status).toBe('settled')
  })
})

// ─────────────────────────────────────────────────────────────────────────────
// Streaming bodies: a Request/Response body that outgrows the probe crosses
// as a stream handle pumped under flow control; the sandbox reads it through
// `.body` / the body helpers. Small bodies keep the buffered path untouched.
// ─────────────────────────────────────────────────────────────────────────────

describe('streaming bodies', () => {
  let runtime: Runtime

  beforeAll(async () => {
    runtime = await createRuntime()
  })

  afterAll(async () => {
    await runtime?.dispose()
  })

  /**
   * A deterministic pattern body of `size` bytes. Built over an explicit
   * ArrayBuffer and left to inference so newer TS libs see the
   * `Uint8Array<ArrayBuffer>` that `BodyInit` demands.
   * @param size body size in bytes
   */
  function patternBytes(size: number) {
    const out = new Uint8Array(new ArrayBuffer(size))
    for (let i = 0; i < size; i++) out[i] = (i * 7 + 13) & 0xFF
    return out
  }

  test('a large call-arg body streams in and survives byte for byte', async () => {
    const size = 1024 * 1024
    const body = patternBytes(size)
    const prefix = await runtime.prepare({
      code: `export default {
        async fetch(request) {
          const bytes = new Uint8Array(await request.arrayBuffer())
          let sum = 0
          for (let i = 0; i < bytes.length; i += 4096) sum = (sum + bytes[i]) % 65536
          return Response.json({ length: bytes.length, sum, streamed: true })
        },
      }`,
    })
    // Two calls: the second exercises a reused warm instance with a fresh
    // stream table.
    for (let round = 0; round < 2; round++) {
      const result = await prefix.call({
        export: 'default.fetch',
        args: [new Request('https://example.com/upload', { method: 'POST', body: patternBytes(size) })],
      })
      expect(result.ok).toBe(true)
      if (!result.ok)
        return
      const report = await (result.value as Response).json() as { length: number, sum: number }
      expect(report.length).toBe(size)
      let sum = 0
      for (let i = 0; i < body.length; i += 4096) sum = (sum + body[i]!) % 65536
      expect(report.sum).toBe(sum)
    }
    await prefix.dispose()
  })

  test('a large bridge-response body streams and is readable incrementally', async () => {
    const size = 512 * 1024
    const result = await runtime.run({
      code: `
        const res = await fetchUpstream()
        const streamed = res.body !== null
        let chunks = 0
        let total = 0
        for await (const chunk of res.body) { chunks++; total += chunk.byteLength }
        export default { streamed, chunks, total }
      `,
      globals: {
        fetchUpstream: async () => new Response(patternBytes(size)),
      },
      limits: { maxBridgeCalls: 2 },
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    const out = result.exports.default as { streamed: boolean, chunks: number, total: number }
    expect(out.streamed).toBe(true)
    expect(out.total).toBe(size)
    // Flow control caps a chunk at 64 KiB, so a 512 KiB body needs several.
    expect(out.chunks).toBeGreaterThan(1)
  })

  test('a small body keeps the buffered path (no stream, body getter is null)', async () => {
    const result = await runtime.run({
      code: `
        const res = await give()
        export default { hasStream: res.body !== null, text: await res.text() }
      `,
      globals: { give: async () => new Response('small enough') },
      limits: { maxBridgeCalls: 2 },
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports.default).toEqual({ hasStream: false, text: 'small enough' })
  })

  test('cancelling releases the host source', async () => {
    let cancelled = false
    const endless = (): ReadableStream<Uint8Array> => new ReadableStream({
      pull(controller) {
        controller.enqueue(new Uint8Array(16 * 1024))
      },
      cancel() {
        cancelled = true
      },
    })
    const result = await runtime.run({
      code: `
        const res = await give()
        const reader = res.body.getReader()
        const first = await reader.read()
        await reader.cancel('done early')
        export default { got: first.value.byteLength > 0 }
      `,
      globals: { give: async () => new Response(endless()) },
      limits: { maxBridgeCalls: 2 },
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports.default).toEqual({ got: true })
    // The cancel frame reaches the pump asynchronously.
    await new Promise((resolve) => {
      setTimeout(resolve, 100)
    })
    expect(cancelled).toBe(true)
  })

  test('a mid-stream source failure rejects the read catchably', async () => {
    let pulls = 0
    const failing = (): ReadableStream<Uint8Array> => new ReadableStream({
      pull(controller) {
        pulls++
        if (pulls > 3)
          throw new Error('disk on fire')
        controller.enqueue(new Uint8Array(64 * 1024))
      },
    })
    const result = await runtime.run({
      code: `
        const res = await give()
        let failure = null
        let total = 0
        try {
          for await (const chunk of res.body) total += chunk.byteLength
        } catch (e) { failure = e.message }
        export default { failure, gotSome: total > 0 }
      `,
      globals: { give: async () => new Response(failing()) },
      limits: { maxBridgeCalls: 2 },
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    const out = result.exports.default as { failure: string, gotSome: boolean }
    expect(out.gotSome).toBe(true)
    expect(out.failure).toContain('disk on fire')
  })

  test('a streamed body keeps flowing during the waitUntil grace phase', async () => {
    const size = 1024 * 1024
    const started = Date.now()
    const result = await runtime.run({
      code: `
        const res = await give()
        waitUntil((async () => {
          const bytes = new Uint8Array(await res.arrayBuffer())
          console.log('drained:' + bytes.length)
        })())
        export default 'early'
      `,
      globals: { give: async () => new Response(patternBytes(size)) },
      limits: { maxBridgeCalls: 2, graceMs: 10_000 },
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports.default).toBe('early')
    const report = await result.waitUntil!
    // The 1 MiB body crosses the 256 KiB credit window four times during
    // grace: a stalled StreamPull loop would park until graceMs instead.
    expect(report.status).toBe('settled')
    expect(report.stdout).toEqual([`drained:${size}`])
    expect(Date.now() - started).toBeLessThan(5_000)
  })

  test('a failed run releases the host body source', async () => {
    let cancelled = false
    const endless = (): ReadableStream<Uint8Array> => new ReadableStream({
      pull(controller) {
        controller.enqueue(new Uint8Array(16 * 1024))
      },
      cancel() {
        cancelled = true
      },
    })
    const result = await runtime.run({
      code: `
        const res = await give()
        res.body.getReader() // hydrated and held
        throw new Error('handler exploded')
      `,
      globals: { give: async () => new Response(endless()) },
      limits: { maxBridgeCalls: 2 },
    })
    expect(result.ok).toBe(false)
    await new Promise((resolve) => {
      setTimeout(resolve, 100)
    })
    expect(cancelled).toBe(true)
  })

  test('clone() tees a streamed body: both instances read the full body independently', async () => {
    const size = 256 * 1024
    const result = await runtime.run({
      code: `
        const res = await give()
        const copy = res.clone()
        // Interleaved consumption: the copy drains while the original reads,
        // which only works when the two branches are independent.
        const [a, b] = await Promise.all([res.arrayBuffer(), copy.arrayBuffer()])
        const bytesA = new Uint8Array(a)
        const bytesB = new Uint8Array(b)
        let equal = bytesA.length === bytesB.length
        for (let i = 0; equal && i < bytesA.length; i += 1024) equal = bytesA[i] === bytesB[i]
        export default { lenA: bytesA.length, lenB: bytesB.length, equal }
      `,
      globals: { give: async () => new Response(patternBytes(size)) },
      limits: { maxBridgeCalls: 2 },
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports.default).toEqual({ lenA: size, lenB: size, equal: true })
  })

  test('cancelling one tee branch leaves the other readable', async () => {
    const size = 512 * 1024
    const result = await runtime.run({
      code: `
        const res = await give()
        const copy = res.clone()
        await copy.body.cancel('not needed')
        const bytes = new Uint8Array(await res.arrayBuffer())
        export default bytes.length
      `,
      globals: { give: async () => new Response(patternBytes(size)) },
      limits: { maxBridgeCalls: 2 },
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports.default).toBe(size)
  })

  test('reading through .body marks bodyUsed, matching Node', async () => {
    const result = await runtime.run({
      code: `
        const res = await give()
        const before = res.bodyUsed
        const reader = res.body.getReader()
        await reader.read()
        export default { before, after: res.bodyUsed }
      `,
      globals: { give: async () => new Response(patternBytes(256 * 1024)) },
      limits: { maxBridgeCalls: 2 },
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports.default).toEqual({ before: false, after: true })
  })

  test('a Response with a streamed body cannot be returned to the host (clear error)', async () => {
    const result = await runtime.run({
      code: `
        const res = await give()
        export default res
      `,
      globals: { give: async () => new Response(patternBytes(256 * 1024)) },
      limits: { maxBridgeCalls: 2 },
    })
    // Host types keep their loud codec diagnostic: the run fails naming the
    // remedy instead of silently skipping or delivering a gutted Response.
    expect(result.ok).toBe(false)
    if (result.ok || result.status !== 'failed')
      return
    expect(result.error.message).toContain('streamed body')
  })
})

// ─────────────────────────────────────────────────────────────────────────────
// readExports: the deploy-path declaration reader
// ─────────────────────────────────────────────────────────────────────────────

describe('readExports', () => {
  let runtime: Runtime

  beforeAll(async () => {
    runtime = await createRuntime()
  })

  afterAll(async () => {
    await runtime?.dispose()
  })

  test('reads declaration exports; handlers are skipped and reported', async () => {
    const { exports, skippedExports } = await runtime.readExports({
      code: `export const limits = { memoryMb: 128, cpuTimeMs: 500 }
             export const connections = ['db-main']
             export default { async fetch() { return new Response('nope') } }`,
    })
    expect(exports['limits']).toEqual({ memoryMb: 128, cpuTimeMs: 500 })
    expect(exports['connections']).toEqual(['db-main'])
    expect('default' in exports).toBe(false)
    expect(skippedExports).toEqual(['default'])
  })

  test('rejects on a broken module, like prepare()', async () => {
    await expect(runtime.readExports({ code: 'export default (((' }))
      .rejects
      .toMatchObject({ code: 'ERR_COMPILE' })
  })

  test('rejects when the module top level throws', async () => {
    await expect(runtime.readExports({ code: 'throw new Error("deploy-time boom")' }))
      .rejects
      .toMatchObject({ code: 'ERR_USER_CODE', message: 'deploy-time boom' })
  })
})
