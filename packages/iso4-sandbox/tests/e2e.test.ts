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
import type { Sandbox as Runtime } from '../src/types'

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

  test('postfix runs are isolated from each other', async () => {
    const prefix = await runtime.precompile({ code: '' })

    // First run mutates globalThis — second must not see it.
    await prefix.run({ code: 'globalThis.__secret = 42; export default 1' })
    const second = await prefix.run({
      code: 'export default globalThis.__secret ?? "clean"',
    })

    expect(second.ok).toBe(true)
    if (!second.ok)
      return
    expect(second.exports.default).toBe('clean')

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

  test.skip('dynamic resolver can provide source modules on demand', async () => {
    const result = await runtime.run({
      code: `
        import { add } from 'dynamic:math'
        export default add(10, 5)
      `,
      imports: {
        resolve: (specifier) => {
          if (specifier === 'dynamic:math')
            return MATH_UTILS_SOURCE
          return null
        },
      },
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports.default).toBe(15)
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

  test('exporting a function → ERR_EXPORT_NOT_SERIALIZABLE', async () => {
    const result = await runtime.run({
      code: 'export default function() {}',
    })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.code).toBe('ERR_EXPORT_NOT_SERIALIZABLE')
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
    const result = await runtime.run({
      code: `
        const arrays = []
        while (true) { arrays.push(new Uint8Array(1024 * 1024)) }
      `,
      limits: { memoryMb: 32, wallTimeMs: 10_000, cpuTimeMs: 10_000 },
    })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.code).toBe('ERR_MEMORY_LIMIT')
  }, 15_000)

  test('memory limit is enforced — logs emitted before OOM are preserved', async () => {
    const result = await runtime.run({
      code: `
        console.log('before oom')
        const arrays = []
        while (true) { arrays.push(new Uint8Array(1024 * 1024)) }
      `,
      limits: { memoryMb: 32, wallTimeMs: 10_000, cpuTimeMs: 10_000 },
    })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.code).toBe('ERR_MEMORY_LIMIT')
    expect(result.stdout.some((l: string) => l.includes('before oom'))).toBe(true)
  }, 15_000)

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
      limits: { cpuTimeMs: 5000, wallTimeMs: 10000, memoryMb: 128 },
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
    expect(result.error.code).toBe('ERR_ABORTED')
  })

  test.skip('signal aborted during async execution produces ERR_ABORTED', async () => {
    const controller = new AbortController()
    // abort after 100ms; the run waits on a slow bridge call
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
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.code).toBe('ERR_ABORTED')
  })

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
