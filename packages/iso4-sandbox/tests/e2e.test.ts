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
// 7b. Wire boundary contract — data, not behavior (GH #9)
//
// Supported across the boundary: primitives, bigint, string, Uint8Array,
// plain objects/arrays. Everything else fails loudly in BOTH directions
// instead of silently corrupting to `{}`.
// ─────────────────────────────────────────────────────────────────────────────

describe('wire boundary contract', () => {
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
    ['Date', 'new Date(1700000000000)'],
    ['Map', 'new Map([["a", 1]])'],
    ['Set', 'new Set([1, 2, 3])'],
    ['RegExp', '/abc/g'],
    ['ArrayBuffer', 'new ArrayBuffer(8)'],
    ['Float32Array', 'new Float32Array([1, 2])'],
  ])('exporting a %s → ERR_EXPORT_NOT_SERIALIZABLE', async (_name, expr) => {
    const result = await runtime.run({ code: `export default ${expr}` })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.code).toBe('ERR_EXPORT_NOT_SERIALIZABLE')
  })

  test('builtin nested inside a plain object also fails loudly', async () => {
    const result = await runtime.run({
      code: 'export default { when: new Date() }',
    })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.code).toBe('ERR_EXPORT_NOT_SERIALIZABLE')
  })

  test('host handler returning a Date → ERR_HOST_BRIDGE', async () => {
    const result = await runtime.run({
      code: 'export default await now()',
      globals: { now: async () => new Date() },
    })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.code).toBe('ERR_HOST_BRIDGE')
  })

  test('host handler returning a class instance → ERR_HOST_BRIDGE', async () => {
    class Row {
      value = 1
    }
    const result = await runtime.run({
      code: 'export default await fetchRow()',
      globals: { fetchRow: async () => new Row() },
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
    // The pattern from #22: rebuild an equivalent error in-sandbox. Since
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
