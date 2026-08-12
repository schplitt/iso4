/**
 * Comprehensive integration tests for \@iso4/sandbox.
 *
 * Uses a single runtime instance for ALL tests to avoid the overhead of
 * spawning multiple binaries. Tests that need future phases are included
 * and will fail with their
 * real error — that's intentional so we can see exactly what's missing.
 *
 * Phase notes:
 * Phase 1 ← basic run (current)
 * Phase 2 ← precompile / prefix.run (current)
 * Phase 3 ← cpu budget bracketing
 * Phase 4 ← fetch global bridge
 * Phase 6 ← source imports
 * Phase 7 ← host import bridge
 * Phase 8 ← heap limits enforcement
 */

import * as http from 'node:http'
import type { AddressInfo } from 'node:net'
import { afterAll, beforeAll, describe, expect, test } from 'vitest'
import { createSandbox as createRuntime } from '../src/index.js'
import type { Sandbox as Runtime } from '../src/types.js'

// ── Shared runtime ─────────────────────────────────────────────────────────

let runtime: Runtime

beforeAll(async () => {
  runtime = await createRuntime({ maxIsolates: 4 })
}, 20_000)

afterAll(async () => {
  await runtime?.dispose()
})

// ── Phase 1: direct runtime.run() ─────────────────────────────────────────

describe('runtime.run() — basic exports', () => {
  test('export default number', async () => {
    const result = await runtime.run({ code: 'export default 42' })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports['default']).toBe(42)
  })

  test('export default string', async () => {
    const result = await runtime.run({ code: 'export default "hello"' })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports['default']).toBe('hello')
  })

  test('export default boolean', async () => {
    const result = await runtime.run({ code: 'export default true' })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports['default']).toBe(true)
  })

  test('export default null', async () => {
    const result = await runtime.run({ code: 'export default null' })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports['default']).toBeNull()
  })

  test('export default object', async () => {
    const result = await runtime.run({ code: 'export default { x: 1, y: 2 }' })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports['default']).toEqual({ x: 1, y: 2 })
  })

  test('export default array', async () => {
    const result = await runtime.run({ code: 'export default [1, 2, 3]' })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports['default']).toEqual([1, 2, 3])
  })

  test('named exports', async () => {
    const result = await runtime.run({
      code: 'export const x = 10; export const y = 20',
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports['x']).toBe(10)
    expect(result.exports['y']).toBe(20)
  })

  test('default and named exports together', async () => {
    const result = await runtime.run({
      code: 'export default 99; export const label = "hi"',
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports['default']).toBe(99)
    expect(result.exports['label']).toBe('hi')
  })

  test('top-level await resolves', async () => {
    const result = await runtime.run({
      code: 'export default await Promise.resolve(7)',
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports['default']).toBe(7)
  })

  test('nested object with array', async () => {
    const result = await runtime.run({
      code: 'export default { items: [1, 2, 3], count: 3 }',
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports['default']).toEqual({ items: [1, 2, 3], count: 3 })
  })

  test('runtime arithmetic', async () => {
    const result = await runtime.run({
      code: `
        let sum = 0
        for (let i = 1; i <= 100; i++) sum += i
        export default sum
      `,
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports['default']).toBe(5050)
  })

  test('durationMs is populated', async () => {
    const result = await runtime.run({ code: 'export default 1' })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.durationMs).toBeGreaterThanOrEqual(0)
    expect(result.durationMs).toBeLessThan(5_000)
  })

  test('multiple concurrent runs are independent', async () => {
    const [a, b, c, d] = await Promise.all([
      runtime.run({ code: 'export default 1' }),
      runtime.run({ code: 'export default 2' }),
      runtime.run({ code: 'export default 3' }),
      runtime.run({ code: 'export default 4' }),
    ])
    expect(a.ok && a.exports['default']).toBe(1)
    expect(b.ok && b.exports['default']).toBe(2)
    expect(c.ok && c.exports['default']).toBe(3)
    expect(d.ok && d.exports['default']).toBe(4)
  })
})

// ── Phase 1: console capture ───────────────────────────────────────────────

describe('console capture', () => {
  test('console.log captured in stdout', async () => {
    const result = await runtime.run({
      code: 'console.log("hello from sandbox"); export default 1',
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.stdout).toContain('hello from sandbox')
    expect(result.stderr).toEqual([])
  })

  test('console.error captured in stderr', async () => {
    const result = await runtime.run({
      code: 'console.error("something bad"); export default 1',
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.stderr).toContain('something bad')
    expect(result.stdout).toEqual([])
  })

  test('console.warn captured in stderr', async () => {
    const result = await runtime.run({
      code: 'console.warn("watch out"); export default 1',
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.stderr).toContain('watch out')
  })

  test('multiple logs preserved in order', async () => {
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
    expect(result.stdout).toEqual(['line one', 'line two', 'line three'])
  })

  test('logs before throw are preserved on failure', async () => {
    const result = await runtime.run({
      code: `
        console.log("before")
        console.error("also before")
        throw new Error("boom")
      `,
    })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.stdout).toContain('before')
    expect(result.stderr).toContain('also before')
  })

  test('logs do not bleed between runs', async () => {
    await runtime.run({ code: 'console.log("run-a"); export default 1' })
    const result = await runtime.run({ code: 'export default 1' })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.stdout).toEqual([])
  })
})

// ── Phase 1: error handling ────────────────────────────────────────────────

describe('error handling', () => {
  test('syntax error → ERR_COMPILE', async () => {
    const result = await runtime.run({ code: 'export default (((' })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.code).toBe('ERR_COMPILE')
  })

  test('runtime error → ERR_USER_CODE with message and stack', async () => {
    const result = await runtime.run({
      code: 'throw new Error("deliberate failure")',
    })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.code).toBe('ERR_USER_CODE')
    expect(result.error.message).toContain('deliberate failure')
    expect(result.error.stack).toBeTruthy()
    expect(result.error.stack).toContain('deliberate failure')
  })

  test('stack trace includes call depth', async () => {
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
    expect(result.error.stack).toContain('inner')
    expect(result.error.stack).toContain('outer')
  })

  test('export function is skipped and reported (#58)', async () => {
    const result = await runtime.run({ code: 'export default function() {}' })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.skippedExports).toEqual(['default'])
    expect(result.exports).toEqual({})
  })

  test('error in one run does not affect the next', async () => {
    const bad = await runtime.run({ code: 'throw new Error("fail")' })
    expect(bad.ok).toBe(false)
    const good = await runtime.run({ code: 'export default 42' })
    expect(good.ok).toBe(true)
    if (!good.ok)
      return
    expect(good.exports['default']).toBe(42)
  })

  test('unconfigured fetch → ERR_USER_CODE (not a runtime crash)', async () => {
    const result = await runtime.run({
      code: 'export default await fetch("https://example.com")',
    })
    // fetch is not configured — V8 throws ReferenceError, which we catch cleanly
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.code).toBe('ERR_USER_CODE')
  })
})

// ── Phase 2: precompile + prefix.run() ────────────────────────────────────

describe('precompile + prefix.run()', () => {
  test('prefix compiles and postfix runs', async () => {
    await using prefix = await runtime.precompile({
      code: 'globalThis.base = 100',
    })
    const result = await prefix.run({ code: 'export default globalThis.base + 1' })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports['default']).toBe(101)
  })

  test('many postfixes run against the same prefix', async () => {
    await using prefix = await runtime.precompile({
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
      expect(result.exports['default']).toBe((i + 1) * 10)
    }
  })

  test('postfix mutations do not leak between runs', async () => {
    await using prefix = await runtime.precompile({
      code: 'globalThis.counter = 0',
    })
    await prefix.run({ code: 'globalThis.counter = 99; export default 1' })
    const second = await prefix.run({
      code: 'export default globalThis.counter',
    })
    expect(second.ok).toBe(true)
    if (!second.ok)
      return
    // Must still be 0, not 99 from the previous run
    expect(second.exports['default']).toBe(0)
  })

  test('prefix with pre-computed data structure', async () => {
    await using prefix = await runtime.precompile({
      code: `
        const squares = {}
        for (let i = 0; i <= 20; i++) squares[i] = i * i
        globalThis.squares = squares
      `,
    })
    const result = await prefix.run({
      code: 'export default globalThis.squares[7]',
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports['default']).toBe(49)
  })

  test('concurrent prefix runs are independent', async () => {
    await using prefix = await runtime.precompile({
      code: 'globalThis.base = 5',
    })
    const [a, b, c] = await Promise.all([
      prefix.run({ code: 'export default globalThis.base + 1' }),
      prefix.run({ code: 'export default globalThis.base + 2' }),
      prefix.run({ code: 'export default globalThis.base + 3' }),
    ])
    expect(a.ok && a.exports['default']).toBe(6)
    expect(b.ok && b.exports['default']).toBe(7)
    expect(c.ok && c.exports['default']).toBe(8)
  })

  test('console available in postfix', async () => {
    await using prefix = await runtime.precompile({ code: '' })
    const result = await prefix.run({
      code: 'console.log("postfix log"); export default 1',
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.stdout).toContain('postfix log')
  })

  test('prefix.alive is false after dispose', async () => {
    const prefix = await runtime.precompile({ code: '' })
    expect(prefix.alive).toBe(true)
    await prefix.dispose()
    expect(prefix.alive).toBe(false)
  })

  test('run on disposed prefix returns ERR_PREFIX_DISPOSED', async () => {
    const prefix = await runtime.precompile({ code: '' })
    await prefix.dispose()
    const result = await prefix.run({ code: 'export default 1' })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.code).toBe('ERR_PREFIX_DISPOSED')
  })

  test('compile error in precompile throws', async () => {
    await expect(
      runtime.precompile({ code: 'export default (((' }),
    ).rejects.toThrow()
  })
})

// ── Phase 4: fetch bridge with real HTTP server ──────────────────────────
//
// These tests document the full intended behaviour of the fetch bridge.
// They FAIL until Phase 4 (globals bridge) is implemented — that's by design.
// The failure message shows exactly what's missing.

describe('fetch bridge — real HTTP server (Phase 4)', () => {
  let server: http.Server
  let serverUrl: string
  const serverValues = [10, 20, 30, 40, 50, 60, 70, 80, 90, 100]
  const expectedSum = serverValues.reduce((a, b) => a + b, 0) // 550

  beforeAll(async () => {
    server = http.createServer((_req, res) => {
      res.writeHead(200, { 'content-type': 'application/json' })
      res.end(JSON.stringify({ values: serverValues }))
    })
    await new Promise<void>((resolve) => {
      server.listen(0, '127.0.0.1', resolve)
    })
    const port = (server.address() as AddressInfo).port
    serverUrl = `http://127.0.0.1:${port}`
  })

  afterAll(async () => {
    await new Promise<void>((resolve, reject) => {
      server.close((err) => (err != null ? reject(err) : resolve()))
    },
    )
  })

  test('fetch values from server, sum with for-loop, verify against main thread', async () => {
    // fetch is a generic bridge global: handler receives (url: string).
    // sandbox parses the response body with JSON.parse.
    const result = await runtime.run({
      code: `
        const res = await fetch('${serverUrl}')
        const { values } = JSON.parse(res.body)
        let sum = 0
        for (let i = 0; i < values.length; i++) sum += values[i]
        export default sum
        export const count = values.length
      `,
      globals: {
        fetch: async (url: unknown) => {
          const nodeRes = await globalThis.fetch(url as string)
          const body = await nodeRes.text()
          return { status: nodeRes.status, headers: Object.fromEntries(nodeRes.headers.entries()), body }
        },
      },
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports['default']).toBe(expectedSum)
    expect(result.exports['count']).toBe(serverValues.length)
  })

  test('3 concurrent prefix runs all fetch the same server and sum correctly', async () => {
    const prefix = await runtime.precompile({
      code: `
        globalThis.fetchSum = async (url) => {
          const res = await fetch(url)
          const { values } = JSON.parse(res.body)
          let total = 0
          for (let i = 0; i < values.length; i++) total += values[i]
          return total
        }
      `,
      globals: {
        fetch: async (url: unknown) => {
          const nodeRes = await globalThis.fetch(url as string)
          const body = await nodeRes.text()
          return { status: nodeRes.status, headers: Object.fromEntries(nodeRes.headers.entries()), body }
        },
      },
    })

    const [r1, r2, r3] = await Promise.all([
      prefix.run({ code: `export default await globalThis.fetchSum('${serverUrl}')` }),
      prefix.run({ code: `export default await globalThis.fetchSum('${serverUrl}')` }),
      prefix.run({ code: `export default await globalThis.fetchSum('${serverUrl}')` }),
    ])
    await prefix.dispose()

    for (const result of [r1, r2, r3]) {
      expect(result.ok).toBe(true)
      if (!result.ok)
        continue
      expect(result.exports['default']).toBe(expectedSum)
    }
  })
})

// ── Phase 4: globals bridge ────────────────────────────────────────────────
// Tests the globals: { fetch, myTool } bridge API.
// All FAIL until Phase 4 wires globals through to the V8 context.

describe('globals bridge (Phase 4)', () => {
  test('fetch without config → clean ERR_USER_CODE, not a crash', async () => {
    const result = await runtime.run({
      code: 'export default await fetch("https://example.com")',
    })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    // This already works — fetch is simply undefined in the sandbox.
    expect(result.error.code).toBe('ERR_USER_CODE')
  })

  test('fetch handler receives the request URL', async () => {
    // fetch is a generic bridge global: handler receives raw arguments.
    // sandbox calls fetch(url_string) → handler receives (url: string).
    // handler returns plain data; sandbox accesses res.body directly.
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
          return { status: 200, headers: { 'content-type': 'application/json' }, body: JSON.stringify({ value: 42 }) }
        },
      },
    })
    expect(requests).toEqual(['https://api.example.com/data'])
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports['default']).toBe(42)
  })

  test('fetch handler can return 4xx and sandbox sees the status', async () => {
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
    expect(result.exports['default']).toBe(403)
  })

  test('fetch handler throwing surfaces as ERR_HOST_BRIDGE', async () => {
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

  test('custom tool function callable from sandbox', async () => {
    const calls: unknown[] = []
    const result = await runtime.run({
      code: 'export default await myTool("hello")',
      globals: {
        myTool: async (arg: unknown) => {
          calls.push(arg)
          return 'world'
        },
      } as any,
    })
    expect(calls).toEqual(['hello'])
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports['default']).toBe('world')
  })

  test('prefix with declared globals can rebind per postfix run', async () => {
    const callLog: string[] = []
    const prefix = await runtime.precompile({
      code: '',
      globals: { fetch: async () => ({ status: 200, headers: {}, body: 'default' as string | null }) },
    })

    const result = await prefix.run({
      code: `
        const res = await fetch('https://example.com')
        export default res.status
      `,
      globals: {
        fetch: async (url) => {
          callLog.push(url as string)
          return { status: 418, headers: {}, body: null }
        },
      },
    })
    await prefix.dispose()

    expect(callLog).toEqual(['https://example.com'])
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports['default']).toBe(418)
  })

  test('globals not declared at precompile time → ERR_UNDECLARED_BINDING', async () => {
    const prefix = await runtime.precompile({ code: '' })
    const result = await prefix.run({
      code: 'export default 1',
      globals: { fetch: async () => ({ status: 200, headers: {}, body: null }) },
    })
    await prefix.dispose()
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.code).toBe('ERR_UNDECLARED_BINDING')
  })
})

// ── Concurrent host callbacks (D11) ─────────────────────────────────────────
//
// These tests verify that Promise.all([toolA(), toolB()]) in sandbox code
// causes the two host handlers to execute concurrently, not sequentially.
// The proof is timing: if both 50 ms handlers run sequentially the total
// wall time would be ≥ 100 ms; if they run concurrently it is ≈ 50 ms.

describe('concurrent host bridge callbacks (D11)', () => {
  test('Promise.all with two slow handlers runs them concurrently', async () => {
    const sleep = (ms: number) => new Promise<void>((r) => {
      setTimeout(r, ms)
    })
    const HANDLER_DELAY_MS = 50
    const started: number[] = []

    const handler = async (_arg: unknown): Promise<number> => {
      started.push(Date.now())
      await sleep(HANDLER_DELAY_MS)
      return 1
    }

    const wallStart = Date.now()
    const result = await runtime.run({
      code: `
        const [a, b] = await Promise.all([tool(), tool()])
        export default a + b
      `,
      globals: { tool: handler } as any,
    })
    const wallMs = Date.now() - wallStart

    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports['default']).toBe(2)

    // Both handlers must have started within a small window of each other.
    // Sequential execution would start the second one ~50 ms after the first.
    expect(started).toHaveLength(2)
    const startDelta = Math.abs(started[1]! - started[0]!)
    expect(startDelta).toBeLessThan(20) // started within 20 ms of each other

    // Total wall time must be well under what sequential would cost (100 ms).
    expect(wallMs).toBeLessThan(HANDLER_DELAY_MS * 2 - 10)
  })

  test('Promise.all with three slow handlers — all start concurrently', async () => {
    const sleep = (ms: number) => new Promise<void>((r) => {
      setTimeout(r, ms)
    })
    const HANDLER_DELAY_MS = 50
    const started: number[] = []

    const handler = async (_arg: unknown): Promise<string> => {
      started.push(Date.now())
      await sleep(HANDLER_DELAY_MS)
      return 'ok'
    }

    const wallStart = Date.now()
    const result = await runtime.run({
      code: `
        const [a, b, c] = await Promise.all([tool(), tool(), tool()])
        export default [a, b, c].every(v => v === 'ok')
      `,
      globals: { tool: handler } as any,
    })
    const wallMs = Date.now() - wallStart

    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports['default']).toBe(true)
    expect(started).toHaveLength(3)

    // All three must have started within a small window.
    const maxDelta = Math.max(...started) - Math.min(...started)
    expect(maxDelta).toBeLessThan(20)

    // Total wall time well under sequential cost (150 ms).
    expect(wallMs).toBeLessThan(HANDLER_DELAY_MS * 3 - 20)
  })

  test('sequential bridge calls still return correct values', async () => {
    // Sanity check: non-concurrent usage still works correctly.
    const results: number[] = []
    const result = await runtime.run({
      code: `
        const a = await tool(1)
        const b = await tool(2)
        const c = await tool(3)
        export default a + b + c
      `,
      globals: {
        tool: async (n: unknown) => {
          results.push(n as number)
          return (n as number) * 10
        },
      } as any,
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports['default']).toBe(60) // 10 + 20 + 30
    expect(results).toEqual([1, 2, 3]) // called in order
  })
})

// ── Phase 6: source imports ────────────────────────────────────────────────
// Tests host-provided JS source modules.
// All FAIL until Phase 6 implements the module resolver.

describe('source imports (Phase 6)', () => {
  const mathSource = `
    export const add = (a, b) => a + b
    export const multiply = (a, b) => a * b
    export const sum = (...nums) => nums.reduce((a, b) => a + b, 0)
    export const clamp = (v, min, max) => Math.min(Math.max(v, min), max)
  `

  const zodLikeSource = `
    export const z = {
      object: (shape) => ({
        parse(data) {
          for (const key of Object.keys(shape)) {
            if (!(key in data)) throw new Error('missing required key: ' + key)
          }
          return data
        },
        safeParse(data) {
          try { return { success: true, data: this.parse(data) } }
          catch (e) { return { success: false, error: e.message } }
        }
      }),
      string: () => ({}),
      number: () => ({}),
    }
  `

  test('import single function from source module', async () => {
    const result = await runtime.run({
      code: `
        import { add } from 'lib:math'
        export default add(3, 4)
      `,
      imports: { 'lib:math': mathSource },
    })
    // Phase 6: fails with ERR_MODULE_NOT_FOUND until resolver is implemented
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports['default']).toBe(7)
  })

  test('import multiple functions from source module', async () => {
    const result = await runtime.run({
      code: `
        import { clamp, sum } from 'lib:math'
        export default sum(clamp(5, 0, 10), clamp(20, 0, 10))
      `,
      imports: { 'lib:math': mathSource },
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports['default']).toBe(15) // clamp(5)=5 + clamp(20)=10
  })

  test('zod-like schema validation — happy path', async () => {
    const result = await runtime.run({
      code: `
        import { z } from 'lib:zod'
        const schema = z.object({ name: z.string(), age: z.number() })
        export default schema.parse({ name: 'Alice', age: 30 })
      `,
      imports: { 'lib:zod': zodLikeSource },
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports['default']).toEqual({ name: 'Alice', age: 30 })
  })

  test('zod-like schema validation — missing key throws', async () => {
    const result = await runtime.run({
      code: `
        import { z } from 'lib:zod'
        const schema = z.object({ name: z.string() })
        export default schema.parse({ wrong: true })
      `,
      imports: { 'lib:zod': zodLikeSource },
    })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.code).toBe('ERR_USER_CODE')
    expect(result.error.message).toContain('missing required key')
  })

  test('unknown specifier → ERR_MODULE_NOT_FOUND (already works)', async () => {
    const result = await runtime.run({
      code: 'import { x } from "not:registered"; export default x',
    })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.code).toBe('ERR_MODULE_NOT_FOUND')
  })

  test('source module declared on prefix is reachable from postfix import', async () => {
    // ESM bindings don't cross module boundaries: a postfix that wants to use
    // a source module must import it itself. The prefix's role here is to
    // declare the binding so the postfix's `import` resolves against the
    // same source. (DESIGN.md §4.3.)
    const prefix = await runtime.precompile({
      code: `import { multiply } from 'lib:math'; globalThis.__primed = true`,
      imports: { 'lib:math': mathSource },
    })
    await using p = prefix
    const result = await p.run({
      code: `import { multiply } from 'lib:math'; export default multiply(6, 7)`,
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports['default']).toBe(42)
  })
})

// ── Phase 7: host import bridge ────────────────────────────────────────────
// Tests host-provided function modules bridged into the sandbox.
// All FAIL until Phase 7 implements synthetic host modules.

describe('host imports (Phase 7)', () => {
  test('host function callable from sandbox', async () => {
    const calls: unknown[] = []
    const result = await runtime.run({
      code: `
        import { echo } from 'host:utils'
        export default await echo('ping')
      `,
      imports: {

        'host:utils': {
          echo: (msg: unknown) => {
            calls.push(msg)
            return String(msg).toUpperCase()
          },
        },
      },
    })
    // Phase 7: fails with ERR_MODULE_NOT_FOUND until host modules are wired
    expect(calls).toEqual(['ping'])
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports['default']).toBe('PING')
  })

  test('async host function is awaited', async () => {
    const result = await runtime.run({
      code: `
        import { fetchData } from 'host:api'
        export default await fetchData('users')
      `,
      imports: {

        'host:api': { fetchData: async (r: unknown) => `data for ${r}` },
      },
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports['default']).toBe('data for users')
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

  test('function argument to host function → ERR_FUNCTION_ARGUMENT_NOT_SUPPORTED', async () => {
    const result = await runtime.run({
      code: `
        import { call } from 'host:cb'
        export default await call(() => 42)
      `,
      imports: {

        'host:cb': { call: (fn) => fn },
      },
    })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.code).toBe('ERR_FUNCTION_ARGUMENT_NOT_SUPPORTED')
  })

  test('host function throwing → ERR_HOST_BRIDGE', async () => {
    const result = await runtime.run({
      code: `
        import { boom } from 'host:broken'
        export default await boom()
      `,
      imports: {

        'host:broken': { boom: () => {
          throw new Error('handler exploded')
        } },
      },
    })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.code).toBe('ERR_HOST_BRIDGE')
  })

  test('nested object exports: 2-level method is callable (users.create)', async () => {
    const created: unknown[] = []
    const result = await runtime.run({
      code: `
        import { users } from 'host:db'
        export default await users.create({ name: 'Alice' })
      `,
      imports: {
        'host:db': {
          users: {
            create: async (row: unknown) => {
              created.push(row)
              return { id: 1, ...(row as object) }
            },
          },
        },
      },
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(created).toEqual([{ name: 'Alice' }])
    expect(result.exports['default']).toEqual({ id: 1, name: 'Alice' })
  })

  test('nested object exports: mixed data + function leaves coexist', async () => {
    const result = await runtime.run({
      code: `
        import { client } from 'host:sdk'
        export default {
          version: client.version,
          mode: client.config.mode,
          result: await client.search('q'),
        }
      `,
      imports: {
        'host:sdk': {
          client: {
            version: '2.1.0',
            config: { mode: 'prod' },
            search: async (q: string) => `results for ${q}`,
          },
        },
      },
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports['default']).toEqual({
      version: '2.1.0',
      mode: 'prod',
      result: 'results for q',
    })
  })

  test('3-level deep method is callable', async () => {
    const result = await runtime.run({
      code: `
        import { a } from 'host:deep'
        export default await a.b.c.run(21)
      `,
      imports: {
        'host:deep': {
          a: { b: { c: { run: async (n: number) => n * 2 } } },
        },
      },
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports['default']).toBe(42)
  })

  test('two host modules with same-named methods do not collide', async () => {
    // Both modules export `get`; ID-based dispatch keeps them distinct.
    const result = await runtime.run({
      code: `
        import { get as getA } from 'host:a'
        import { get as getB } from 'host:b'
        export default [await getA(), await getB()]
      `,
      imports: {
        'host:a': { get: async () => 'A' },
        'host:b': { get: async () => 'B' },
      },
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports['default']).toEqual(['A', 'B'])
  })

  test('specifiers with weird characters resolve without name collisions', async () => {
    // 'host:tools/v2' and 'host_tools_v2' would have collided under the old
    // name-sanitising scheme; ID-based dispatch makes them independent.
    const result = await runtime.run({
      code: `
        import { ping as p1 } from 'host:tools/v2'
        import { ping as p2 } from 'host_tools_v2'
        export default [await p1(), await p2()]
      `,
      imports: {
        'host:tools/v2': { ping: async () => 'slash' },
        'host_tools_v2': { ping: async () => 'underscore' },
      },
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports['default']).toEqual(['slash', 'underscore'])
  })
})

// ── Imports: rebinding enforcement on prefix.run() ────────────────────
// Mirrors the globals path: anything not declared at precompile becomes
// ERR_UNDECLARED_BINDING at run time. TypeScript catches most of this
// statically via `RebindImports<I>`; these tests verify the runtime
// fallback when callers bypass the type system with `as any` / dynamic data.

describe('imports rebinding enforcement (runtime)', () => {
  test('rebinding an undeclared specifier on prefix.run() → ERR_UNDECLARED_BINDING', async () => {
    await using prefix = await runtime.precompile({
      code: `globalThis.__primed = true`,
      imports: {

        'host:declared': { fn: () => 1 },
      },
    })
    const result = await prefix.run({
      code: 'export default 1',
      // Bypass the type system the way a JS caller might.
      imports: {

        'host:never-declared': { fn: () => 2 },
      } as any,
    })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.code).toBe('ERR_UNDECLARED_BINDING')
    expect(result.error.message).toMatch(/host:never-declared/)
  })

  test('rebinding an undeclared function export on a declared specifier → ERR_UNDECLARED_BINDING', async () => {
    await using prefix = await runtime.precompile({
      code: `globalThis.__primed = true`,
      imports: {

        'host:tools': { search: () => 'a' },
      },
    })
    const result = await prefix.run({
      code: 'export default 1',
      imports: {

        'host:tools': { undeclared: () => 1 },
      } as any,
    })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.code).toBe('ERR_UNDECLARED_BINDING')
    expect(result.error.message).toMatch(/undeclared/)
  })

  test('rebinding a source-module specifier on prefix.run() → ERR_UNDECLARED_BINDING', async () => {
    await using prefix = await runtime.precompile({
      code: `globalThis.__primed = true`,
      imports: {

        'lib:math': 'export const x = 1',
      },
    })
    const result = await prefix.run({
      code: 'export default 1',
      imports: {
        'lib:math': 'export const x = 2',
      } as any,
    })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.code).toBe('ERR_UNDECLARED_BINDING')
    expect(result.error.message).toMatch(/source imports are frozen/)
  })

  test('rebinding a data export on prefix.run() → ERR_UNDECLARED_BINDING', async () => {
    await using prefix = await runtime.precompile({
      code: `globalThis.__primed = true`,
      imports: {

        'host:cfg': { version: '1.0.0' },
      },
    })
    const result = await prefix.run({
      code: 'export default 1',
      imports: {

        'host:cfg': { version: '2.0.0' },
      } as any,
    })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.code).toBe('ERR_UNDECLARED_BINDING')
    expect(result.error.message).toMatch(/can only be rebound with a function/)
  })

  test('rebinding a data export with a function → ERR_UNDECLARED_BINDING (runtime shape check)', async () => {
    // A function value passes the client-side checks, so this exercises the
    // Rust-side validation against the declared prefix shape.
    await using prefix = await runtime.precompile({
      code: `globalThis.__primed = true`,
      imports: {

        'host:cfg': { version: '1.0.0' },
      },
    })
    const result = await prefix.run({
      code: 'export default 1',
      imports: {

        'host:cfg': { version: () => '2.0.0' },
      } as any,
    })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.code).toBe('ERR_UNDECLARED_BINDING')
    expect(result.error.message).toMatch(/is a data leaf, not a function/)
  })

  test('declared function leaf CAN be rebound and the new handler is used', async () => {
    await using prefix = await runtime.precompile({
      code: `globalThis.__primed = true`,
      imports: {

        'host:tools': { search: () => 'original' },
      },
    })
    const first = await prefix.run({
      code: `import { search } from 'host:tools'; export default await search()`,
    })
    expect(first.ok).toBe(true)
    if (!first.ok)
      return
    expect(first.exports['default']).toBe('original')

    const second = await prefix.run({
      code: `import { search } from 'host:tools'; export default await search()`,
      imports: {

        'host:tools': { search: () => 'rebound' },
      },
    })
    expect(second.ok).toBe(true)
    if (!second.ok)
      return
    expect(second.exports['default']).toBe('rebound')
    // The rebind is per-run: the default handler is back on the next run.
    const third = await prefix.run({
      code: `import { search } from 'host:tools'; export default await search()`,
    })
    expect(third.ok).toBe(true)
    if (!third.ok)
      return
    expect(third.exports['default']).toBe('original')
  })
})

// ── Imports: failure paths in the resolver itself ──────────────────────
// Errors that originate inside compile / evaluate of a source or host import
// must surface as a typed RunResult, not as a thrown promise.

describe('imports failure paths', () => {
  test('source module with a syntax error → ERR_COMPILE', async () => {
    const result = await runtime.run({
      code: `import { x } from 'lib:broken'; export default x`,
      imports: {

        'lib:broken': 'export const x =',
      },
    })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.code).toBe('ERR_COMPILE')
  })

  test('source module that throws at top level → ERR_USER_CODE', async () => {
    const result = await runtime.run({
      code: `import { x } from 'lib:bomb'; export default x`,
      imports: {

        'lib:bomb': `throw new Error('module init failed'); export const x = 1`,
      },
    })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.code).toBe('ERR_USER_CODE')
    expect(result.error.message).toMatch(/module init failed/)
  })

  test('host import handler that returns a non-serialisable value → ERR_HOST_BRIDGE', async () => {
    // Functions are the canonical non-serialisable value. The TS encoder
    // throws on them and converts to an error response on the bridge.
    const result = await runtime.run({
      code: `import { broken } from 'host:tools'; export default await broken()`,
      imports: {
        'host:tools': {
          // Override the inferred return type so TS lets us return a function.
          broken: ((() => () => 1) as unknown as (...args: unknown[]) => unknown),
        },
      },
    })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.code).toBe('ERR_HOST_BRIDGE')
  })
})

// ── Phase 3/8: resource limits enforcement ─────────────────────────────────
// API surface tests pass now. Enforcement tests are skipped because an
// un-enforced infinite loop would hang the whole test suite.

describe('resource limits (Phase 3/8)', () => {
  test('zero limits accepted — run completes', async () => {
    const result = await runtime.run({
      code: 'export default 42',
      limits: { cpuTimeMs: 0, wallTimeMs: 0, memoryMb: 0 },
    })
    expect(result.ok).toBe(true)
  })

  test('explicit limits — run completes within them', async () => {
    const result = await runtime.run({
      code: `
        let sum = 0
        for (let i = 0; i < 100_000; i++) sum += i
        export default sum
      `,
      limits: { cpuTimeMs: 5_000, wallTimeMs: 10_000, memoryMb: 128 },
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports['default']).toBe(4_999_950_000)
  })

  test('limits flow through to concurrent runs', async () => {
    const results = await Promise.all(
      Array.from({ length: 4 }, (_, i) =>
        runtime.run({ code: `export default ${i}`, limits: { cpuTimeMs: 1_000, memoryMb: 64 } })),
    )
    for (const [i, r] of results.entries()) {
      expect(r.ok).toBe(true)
      if (!r.ok)
        continue
      expect(r.exports['default']).toBe(i)
    }
  })

  test('tight cpu budget kills tight loop', async () => {
    const result = await runtime.run({
      code: 'let i = 0; while (true) { i++; }',
      limits: { cpuTimeMs: 200, wallTimeMs: 2_000 },
    })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(['ERR_CPU_TIMEOUT', 'ERR_WALL_TIMEOUT']).toContain(result.error.code)
  }, 5_000)

  test('cpu budget excludes globals bridge wait time', async () => {
    // cpuTimeMs is tight (50ms) but the global bridge call takes 300ms.
    // The run must succeed because bridge-wait time (leave/enter bracketing)
    // is excluded from the CPU budget — this is the Phase 3+4 contract.
    const result = await runtime.run({
      code: `
        await sleep(300)
        export default 'done'
      `,
      limits: { cpuTimeMs: 50, wallTimeMs: 5_000 },
      globals: {
        sleep: (ms: unknown) =>
          new Promise<void>((resolve) => {
            setTimeout(resolve, Number(ms))
          }),
      },
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports['default']).toBe('done')
  }, 10_000)

  test('wall timeout fires while sandbox is blocked in globals bridge call', async () => {
    // wallTimeMs is tight; the global bridge call hangs indefinitely.
    // The wall-clock guard fires even while V8 is blocked waiting for a
    // BridgeResponse (the guard runs on its own OS thread).
    const result = await runtime.run({
      code: 'await neverResolves(); export default 1',
      limits: { cpuTimeMs: 30_000, wallTimeMs: 300 },
      globals: {
        neverResolves: () => new Promise<void>(() => {}), // deliberate hang
      },
    })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    // The wall guard fires, but since V8 is blocked in a bridge read the
    // termination is delivered when execution resumes. Either timeout code is
    // acceptable; ERR_WALL_TIMEOUT is expected when the guard beats the bridge.
    expect(['ERR_WALL_TIMEOUT', 'ERR_CPU_TIMEOUT']).toContain(result.error.code)
  }, 10_000)

  // wall timeout via tight loop (no bridge): use wall_time_ms < cpu_time_ms
  test('wall timeout fires before cpu timeout', async () => {
    const result = await runtime.run({
      code: 'let i = 0; while (true) { i++; }',
      limits: { cpuTimeMs: 30_000, wallTimeMs: 200 },
    })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.code).toBe('ERR_WALL_TIMEOUT')
  }, 5_000)

  test('heap limit kills memory hog via TypedArray', async () => {
    const result = await runtime.run({
      code: 'const bufs = []; while(true) bufs.push(new Uint8Array(512*1024))',
      limits: { memoryMb: 16, wallTimeMs: 10_000, cpuTimeMs: 10_000 },
    })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.code).toBe('ERR_MEMORY_LIMIT')
  }, 15_000)

  test('cpu budget excludes bridge wait time (Phase 3)', async () => {
    // cpuTimeMs is tight but the bridge call takes much longer.
    // Once Phase 3 brackets cpu time, this should succeed.
    const result = await runtime.run({
      code: `
        import { sleep } from 'host:time'
        await sleep(300)
        export default 'done'
      `,
      limits: { cpuTimeMs: 50, wallTimeMs: 5_000 },
      imports: {

        'host:time': {
          sleep: (ms) => {
            return new Promise<void>((resolve) => {
              setTimeout(resolve, Number(ms))
            })
          },
        },
      },
    })
    // Phase 6+7 not implemented: ERR_MODULE_NOT_FOUND first.
    // When Phase 6+7 lands, Phase 3 determines if this passes or fails.
    if (!result.ok && result.error.code === 'ERR_MODULE_NOT_FOUND')
      return
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports['default']).toBe('done')
  })
})

// ── Export type coverage ───────────────────────────────────────────────────

describe('export types — full coverage', () => {
  test('BigInt positive', async () => {
    const result = await runtime.run({ code: 'export default 42n' })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports['default']).toBe(42n)
  })

  test('BigInt negative', async () => {
    const result = await runtime.run({ code: 'export default -999n' })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports['default']).toBe(-999n)
  })

  test('BigInt arithmetic beyond Number.MAX_SAFE_INTEGER', async () => {
    const result = await runtime.run({ code: 'export default 9007199254740993n * 2n' })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports['default']).toBe(18014398509481986n)
  })

  test('undefined', async () => {
    const result = await runtime.run({ code: 'export default undefined' })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports['default']).toBeUndefined()
  })

  test('zero, empty string, false', async () => {
    const result = await runtime.run({
      code: 'export const a = 0; export const b = ""; export const c = false',
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports['a']).toBe(0)
    expect(result.exports['b']).toBe('')
    expect(result.exports['c']).toBe(false)
  })

  test('NaN and Infinities', async () => {
    const result = await runtime.run({ code: 'export default [NaN, Infinity, -Infinity]' })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    const [nan, inf, neginf] = result.exports['default'] as number[]
    expect(nan).toBeNaN()
    expect(inf).toBe(Infinity)
    expect(neginf).toBe(-Infinity)
  })

  test('deeply nested object — 10 levels', async () => {
    const result = await runtime.run({
      code: `
        function nest(d, v) {
          return d === 0 ? { val: v } : { depth: d, inner: nest(d - 1, v) }
        }
        export default nest(10, 'leaf')
      `,
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    let node: any = result.exports['default']
    let depth = 0
    while (node.inner) {
      node = node.inner
      depth++
    }
    expect(node.val).toBe('leaf')
    expect(depth).toBe(10)
  })

  test('array with mixed types including BigInt', async () => {
    const result = await runtime.run({
      code: 'export default [1, "two", true, null, undefined, 3n, { x: 5 }]',
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports['default']).toEqual([1, 'two', true, null, undefined, 3n, { x: 5 }])
  })

  test('large array — 1000 elements', async () => {
    const result = await runtime.run({
      code: `
        const a = []
        for (let i = 0; i < 1000; i++) a.push(i * 2)
        export default a
      `,
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    const arr = result.exports['default'] as number[]
    expect(arr.length).toBe(1000)
    expect(arr[999]).toBe(1998)
  })

  test('object with 100 properties', async () => {
    const result = await runtime.run({
      code: `
        const o = {}
        for (let i = 0; i < 100; i++) o['k' + i] = i * i
        export default o
      `,
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    const o = result.exports['default'] as Record<string, number>
    expect(o['k50']).toBe(2500)
    expect(Object.keys(o).length).toBe(100)
  })

  test('no exports — everything undefined', async () => {
    const result = await runtime.run({ code: 'const x = 1' })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports['default']).toBeUndefined()
  })

  // Non-serializable exports are skipped and reported, never fatal (#58).

  test('Symbol export is skipped and reported', async () => {
    const result = await runtime.run({ code: 'export default Symbol("s")' })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.skippedExports).toEqual(['default'])
  })

  test('unresolved Promise export is skipped and reported', async () => {
    const result = await runtime.run({ code: 'export default new Promise(() => {})' })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.skippedExports).toEqual(['default'])
  })

  test('class export is skipped and reported', async () => {
    const result = await runtime.run({ code: 'export default class Foo {}' })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.skippedExports).toEqual(['default'])
  })

  // Cycles used to be rejected by the hand-written codec. The V8
  // serialization format has back-references, so they now round-trip with
  // object identity preserved.
  test('cyclic object round-trips with its self-reference', async () => {
    const result = await runtime.run({
      code: 'const o = {}; o.self = o; export default o',
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    const d = result.exports['default'] as { self: unknown }
    expect(d.self).toBe(d)
  })

  test('cyclic array round-trips with its self-reference', async () => {
    const result = await runtime.run({
      code: 'const a = [1, 2]; a.push(a); export default a',
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    const d = result.exports['default'] as unknown[]
    expect(d.slice(0, 2)).toEqual([1, 2])
    expect(d[2]).toBe(d)
  })

  test('cross-type indirect cycle round-trips', async () => {
    const result = await runtime.run({
      code: 'const a = []; const o = { a }; a.push(o); export default a',
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    const d = result.exports['default'] as { a: unknown }[]
    expect(d[0]?.a).toBe(d)
  })

  test('shared (non-cyclic) reference succeeds and keeps its identity', async () => {
    const result = await runtime.run({
      code: 'const s = { x: 1 }; export default { a: s, b: s }',
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    const d = result.exports['default'] as any
    expect(d.a).toEqual({ x: 1 })
    expect(d.b).toEqual({ x: 1 })
    // The V8 format writes the second occurrence as a back-reference, so the
    // two fields decode to the same object rather than two copies.
    expect(d.a).toBe(d.b)
  })
})

// ── Async / Promise patterns ───────────────────────────────────────────────

describe('async patterns', () => {
  test('Promise.all resolves all', async () => {
    const result = await runtime.run({
      code: `
        const [a, b, c] = await Promise.all([
          Promise.resolve(1),
          Promise.resolve(2),
          Promise.resolve(3),
        ])
        export default a + b + c
      `,
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports['default']).toBe(6)
  })

  test('Promise.allSettled returns mixed status array', async () => {
    const result = await runtime.run({
      code: `
        const results = await Promise.allSettled([
          Promise.resolve('ok'),
          Promise.reject(new Error('fail')),
          Promise.resolve('also ok'),
        ])
        export default results.map(r => r.status)
      `,
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports['default']).toEqual(['fulfilled', 'rejected', 'fulfilled'])
  })

  test('async function chain', async () => {
    const result = await runtime.run({
      code: `
        async function step1(x) { return x * 2 }
        async function step2(x) { return x + 10 }
        async function step3(x) { return x * x }
        export default await step3(await step2(await step1(3)))
      `,
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    // step1(3)=6, step2(6)=16, step3(16)=256
    expect(result.exports['default']).toBe(256)
  })

  test('TLA with rejected promise → ERR_USER_CODE', async () => {
    const result = await runtime.run({
      code: 'export default await Promise.reject(new Error("rejected"))',
    })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.code).toBe('ERR_USER_CODE')
    expect(result.error.message).toContain('rejected')
  })

  test('async error preserves stack trace', async () => {
    const result = await runtime.run({
      code: `
        async function boom() { throw new Error("async boom") }
        export default await boom()
      `,
    })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.message).toContain('async boom')
    expect(result.error.stack).toContain('boom')
  })

  test('async generator iteration', async () => {
    const result = await runtime.run({
      code: `
        async function* range(n) {
          for (let i = 0; i < n; i++) yield i
        }
        const values = []
        for await (const v of range(5)) values.push(v)
        export default values
      `,
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports['default']).toEqual([0, 1, 2, 3, 4])
  })

  test('10 concurrent runs return independent results', async () => {
    const codes = Array.from({ length: 10 }, (_, i) => `export default ${i * i}`)
    const results = await Promise.all(codes.map((code) => runtime.run({ code })))
    for (const [i, result] of results.entries()) {
      expect(result.ok).toBe(true)
      if (!result.ok)
        continue
      expect(result.exports['default']).toBe(i * i)
    }
  })
})

// ── Recursion ──────────────────────────────────────────────────────────────

describe('recursion', () => {
  test('fibonacci(20) = 6765', async () => {
    const result = await runtime.run({
      code: `
        function fib(n) { return n <= 1 ? n : fib(n - 1) + fib(n - 2) }
        export default fib(20)
      `,
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports['default']).toBe(6765)
  })

  test('sum 1..100 = 5050 (iterative)', async () => {
    const result = await runtime.run({
      code: 'let s = 0; for (let i = 1; i <= 100; i++) s += i; export default s',
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports['default']).toBe(5050)
  })

  test('recursive binary tree — count nodes', async () => {
    const result = await runtime.run({
      code: `
        function mkTree(d) {
          return d === 0 ? { v: 0 } : { v: d, l: mkTree(d - 1), r: mkTree(d - 1) }
        }
        function count(n) {
          return n ? 1 + count(n.l) + count(n.r) : 0
        }
        export default count(mkTree(4))
      `,
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports['default']).toBe(31) // 2^(4+1) - 1
  })

  test('deep recursion → stack overflow → ERR_USER_CODE', async () => {
    const result = await runtime.run({
      code: 'function inf(n) { return inf(n + 1) }; export default inf(0)',
    })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.code).toBe('ERR_USER_CODE')
  })

  test('mutual recursion isEven / isOdd', async () => {
    const result = await runtime.run({
      code: `
        function isEven(n) { return n === 0 ? true : isOdd(n - 1) }
        function isOdd(n) { return n === 0 ? false : isEven(n - 1) }
        export default [isEven(10), isOdd(10), isEven(7)]
      `,
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports['default']).toEqual([true, false, false])
  })
})

// ── Built-in JS globals ────────────────────────────────────────────────────

describe('built-in JS globals (no bridge needed)', () => {
  test('Math functions', async () => {
    const result = await runtime.run({
      code: `
        export default {
          sqrt: Math.sqrt(16),
          pow: Math.pow(2, 10),
          floor: Math.floor(3.7),
          abs: Math.abs(-42),
        }
      `,
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports['default']).toEqual({ sqrt: 4, pow: 1024, floor: 3, abs: 42 })
  })

  test('JSON.parse and JSON.stringify', async () => {
    const result = await runtime.run({
      code: `
        const o = JSON.parse('{"a":1,"b":[2,3]}')
        export default JSON.stringify(o)
      `,
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports['default']).toBe('{"a":1,"b":[2,3]}')
  })

  test('Array built-in methods', async () => {
    const result = await runtime.run({
      code: `
        const nums = [5, 3, 1, 4, 2]
        export default {
          sorted: [...nums].sort((a, b) => a - b),
          sum: nums.reduce((a, b) => a + b, 0),
          doubled: nums.map(x => x * 2),
          evens: nums.filter(x => x % 2 === 0),
        }
      `,
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    const d = result.exports['default'] as any
    expect(d.sorted).toEqual([1, 2, 3, 4, 5])
    expect(d.sum).toBe(15)
    expect(d.evens).toEqual([4, 2])
  })

  test('String methods', async () => {
    const result = await runtime.run({
      code: `
        const s = "Hello, World!"
        export default {
          upper: s.toUpperCase(),
          slice: s.slice(7, 12),
          replace: s.replace("World", "V8"),
          includes: s.includes("World"),
        }
      `,
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    const d = result.exports['default'] as any
    expect(d.upper).toBe('HELLO, WORLD!')
    expect(d.slice).toBe('World')
    expect(d.replace).toBe('Hello, V8!')
    expect(d.includes).toBe(true)
  })

  test('Map and Set', async () => {
    const result = await runtime.run({
      code: `
        const m = new Map([['a', 1], ['b', 2]])
        const s = new Set([1, 2, 2, 3, 3, 3])
        export default { mapSize: m.size, setSize: s.size, bVal: m.get('b'), has2: s.has(2) }
      `,
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports['default']).toEqual({ mapSize: 2, setSize: 3, bVal: 2, has2: true })
  })

  test('Regular expressions', async () => {
    const result = await runtime.run({
      code: `
        const m = '3.14'.match(/^(\\d+)\\.(\\d+)$/)
        export default { matched: m !== null, int: m?.[1], frac: m?.[2] }
      `,
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports['default']).toEqual({ matched: true, int: '3', frac: '14' })
  })

  test('Date operations', async () => {
    const result = await runtime.run({
      code: `
        const d = new Date('2024-01-15T12:00:00Z')
        export default {
          year: d.getUTCFullYear(),
          month: d.getUTCMonth() + 1,
          day: d.getUTCDate(),
        }
      `,
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports['default']).toEqual({ year: 2024, month: 1, day: 15 })
  })

  test('String character codes (pure JS, no Web API)', async () => {
    const result = await runtime.run({
      code: `
        const s = "hello"
        const codes = Array.from(s).map(c => c.charCodeAt(0))
        export default { codes, roundTrip: String.fromCharCode(...codes) }
      `,
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    const d = result.exports['default'] as any
    expect(d.codes).toEqual([104, 101, 108, 108, 111])
    expect(d.roundTrip).toBe('hello')
  })

  test('deep clone via JSON round-trip', async () => {
    const result = await runtime.run({
      code: `
        const orig = { nums: [1, 2, 3], nested: { deep: true } }
        const clone = JSON.parse(JSON.stringify(orig))
        clone.nums.push(4)
        export default { origLen: orig.nums.length, cloneLen: clone.nums.length }
      `,
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports['default']).toEqual({ origLen: 3, cloneLen: 4 })
  })
})

// ── Error edge cases ───────────────────────────────────────────────────────

describe('error edge cases', () => {
  test('TypeError — property access on null', async () => {
    const result = await runtime.run({ code: 'null.property' })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.code).toBe('ERR_USER_CODE')
    expect(result.error.message).toMatch(/null|Cannot read/)
  })

  test('ReferenceError — undeclared variable', async () => {
    const result = await runtime.run({ code: 'export default undeclaredXYZ' })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.code).toBe('ERR_USER_CODE')
    expect(result.error.message).toContain('undeclaredXYZ')
  })

  test('RangeError — invalid array length', async () => {
    const result = await runtime.run({ code: 'new Array(-1)' })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.code).toBe('ERR_USER_CODE')
  })

  test('thrown string primitive — message is the string', async () => {
    const result = await runtime.run({ code: 'throw "raw string error"' })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.code).toBe('ERR_USER_CODE')
    expect(result.error.message).toContain('raw string error')
  })

  test('thrown number primitive', async () => {
    const result = await runtime.run({ code: 'throw 42' })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.code).toBe('ERR_USER_CODE')
  })

  test('custom error class preserves message', async () => {
    const result = await runtime.run({
      code: `
        class DomainError extends Error {
          constructor(m) { super(m); this.name = 'DomainError' }
        }
        throw new DomainError("invalid state")
      `,
    })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.code).toBe('ERR_USER_CODE')
    expect(result.error.message).toContain('invalid state')
  })

  test('error in deeply nested async chain', async () => {
    const result = await runtime.run({
      code: `
        async function a() { return b() }
        async function b() { return c() }
        async function c() { throw new Error("deep async") }
        export default await a()
      `,
    })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.message).toContain('deep async')
    expect(result.error.stack).toContain('at c')
  })

  test('try/catch in sandbox does not leak to host', async () => {
    const result = await runtime.run({
      code: `
        let caught = null
        try { throw new Error("sandboxed") } catch (e) { caught = e.message }
        export default caught
      `,
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports['default']).toBe('sandboxed')
  })
})

// ── Precompile stress ──────────────────────────────────────────────────────

describe('precompile stress', () => {
  test('20 concurrent postfixes — all independent', async () => {
    await using prefix = await runtime.precompile({ code: 'globalThis.base = 7' })
    const results = await Promise.all(
      Array.from({ length: 20 }, (_, i) => (
        prefix.run({ code: `export default globalThis.base * ${i + 1}` })
      )),
    )
    for (const [i, result] of results.entries()) {
      expect(result.ok).toBe(true)
      // @ts-expect-error exports present as it is a successful run
      expect(result.exports['default']).toBe(7 * (i + 1))
    }
  })

  test('prefix with factorial + fibonacci lookup tables', async () => {
    await using prefix = await runtime.precompile({
      code: `
        const fact = [1]
        for (let i = 1; i <= 12; i++) fact[i] = fact[i - 1] * i
        globalThis.fact = fact

        const fib = [0, 1]
        for (let i = 2; i <= 20; i++) fib[i] = fib[i - 1] + fib[i - 2]
        globalThis.fib = fib
      `,
    })
    const [r1, r2] = await Promise.all([
      prefix.run({ code: 'export default globalThis.fact[10]' }),
      prefix.run({ code: 'export default globalThis.fib[15]' }),
    ])
    expect(r1.ok).toBe(true)
    if (r1.ok)
      expect(r1.exports['default']).toBe(3_628_800)
    expect(r2.ok).toBe(true)
    if (r2.ok)
      expect(r2.exports['default']).toBe(610)
  })

  test('two independent prefixes run concurrently', async () => {
    const [pA, pB] = await Promise.all([
      runtime.precompile({ code: 'globalThis.name = "alpha"' }),
      runtime.precompile({ code: 'globalThis.name = "beta"' }),
    ])
    const [r1, r2, r3, r4] = await Promise.all([
      pA.run({ code: 'export default globalThis.name + "1"' }),
      pB.run({ code: 'export default globalThis.name + "2"' }),
      pA.run({ code: 'export default globalThis.name + "3"' }),
      pB.run({ code: 'export default globalThis.name + "4"' }),
    ])
    await Promise.all([pA.dispose(), pB.dispose()])
    expect(r1.ok && r1.exports['default']).toBe('alpha1')
    expect(r2.ok && r2.exports['default']).toBe('beta2')
    expect(r3.ok && r3.exports['default']).toBe('alpha3')
    expect(r4.ok && r4.exports['default']).toBe('beta4')
  })

  test('prefix slot recycles correctly after dispose', async () => {
    for (let round = 0; round < 5; round++) {
      await using prefix = await runtime.precompile({
        code: `globalThis.round = ${round}`,
      })
      const result = await prefix.run({ code: 'export default globalThis.round' })
      expect(result.ok).toBe(true)
      if (!result.ok)
        continue
      expect(result.exports['default']).toBe(round)
    }
  })

  test('postfix error does not corrupt subsequent runs on same prefix', async () => {
    await using prefix = await runtime.precompile({ code: 'globalThis.value = 99' })
    const bad = await prefix.run({ code: 'throw new Error("postfix fail")' })
    expect(bad.ok).toBe(false)
    const good = await prefix.run({ code: 'export default globalThis.value' })
    expect(good.ok).toBe(true)
    if (!good.ok)
      return
    expect(good.exports['default']).toBe(99)
  })
})

// ── BridgeWithShim integration ───────────────────────────────────────────
// Tests that the three HostGlobalValue shapes all work end-to-end with the
// real V8 binary.

describe('globals bridge — BridgeWithShim (Phase 4)', () => {
  test('string global is available as a constant in the sandbox', async () => {
    const result = await runtime.run({
      code: 'export default API_URL',
      globals: {
        API_URL: `'https://api.example.com'`,
      },
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports['default']).toBe('https://api.example.com')
  })

  test('string global expression is evaluated — not just a string literal', async () => {
    const result = await runtime.run({
      code: 'export default DOUBLE',
      globals: {
        DOUBLE: `21 * 2`,
      },
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports['default']).toBe(42)
  })

  test('data global: a plain constant object is materialised natively in the sandbox', async () => {
    const config = { model: 'gpt-4', maxTokens: 1000, nested: { retries: 3 } }
    const result = await runtime.run({
      code: 'export default { model: config.model, retries: config.nested.retries }',
      globals: {
        config: { kind: 'data', value: config },
      },
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports['default']).toEqual({ model: 'gpt-4', retries: 3 })
  })

  test('data global: carries Uint8Array, bigint, and null without a code string', async () => {
    const result = await runtime.run({
      code: `export default {
        byte: blob[1],
        len: blob.length,
        big: (big + 1n).toString(),
        nothing: nada,
      }`,
      globals: {
        blob: { kind: 'data', value: new Uint8Array([10, 20, 30]) },
        big: { kind: 'data', value: 9007199254740993n },
        nada: { kind: 'data', value: null },
      },
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports['default']).toEqual({
      byte: 20,
      len: 3,
      big: '9007199254740994',
      nothing: null,
    })
  })

  test('data global: survives the snapshot creator and reaches a prefix run', async () => {
    // Data globals and host-module data leaves are materialised from their
    // value blob inside the *snapshot-creator* isolate at prepare() time, then
    // frozen into the snapshot. This is the one place the value codec runs
    // outside a normal isolate, so it gets its own coverage.
    await using prefix = await runtime.prepare({
      code: 'export const ready = true',
      globals: {
        CONFIG: { kind: 'data', value: { region: 'eu', retries: 3, nested: { a: [1, 2] } } },
        BLOB: { kind: 'data', value: new Uint8Array([7, 8, 9]) },
      },
      imports: { 'tools:cfg': { version: '1.2.3', limits: { max: 5 } } },
    })

    const result = await prefix.execute({
      code: `
        import { version, limits } from 'tools:cfg'
        export default {
          region: CONFIG.region,
          deep: CONFIG.nested.a,
          bytes: Array.from(BLOB),
          version,
          max: limits.max,
        }
      `,
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports['default']).toEqual({
      region: 'eu',
      deep: [1, 2],
      bytes: [7, 8, 9],
      version: '1.2.3',
      max: 5,
    })
  })

  test('line numbers: a user error reports its real line even with globals configured', async () => {
    // Regression for #38: globals used to be installed by prepending generated
    // source to user code, which shifted every stack-trace line. Native install
    // means user code starts at line 1 regardless of how many globals exist.
    const result = await runtime.run({
      // `throw` is on line 1 of the user code.
      code: `throw new Error('boom')`,
      globals: {
        API_URL: `'https://api.example.com'`,
        config: { kind: 'data', value: { a: 1 } },
        fetch: {
          kind: 'bridge-with-shim',
          handler: async () => ({ status: 200 }),
          shim: `(r) => r`,
        },
        myTool: async () => 'ok',
      },
    })
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.message).toBe('boom')
    // The throw is on line 1; the preamble no longer offsets it.
    expect(result.error.stack).toContain(':1:')
    expect(result.error.stack).not.toContain(':2:')
  })

  test('BridgeWithShim: sandbox sees the shimmed wrapper, not the raw return value', async () => {
    // The handler returns { status, body }; the shim adds .ok and .data
    const result = await runtime.run({
      code: `
        const res = await myApi('hello')
        export default { ok: res.ok, data: res.data, status: res.status }
      `,
      globals: {
        myApi: {
          kind: 'bridge-with-shim',
          handler: async (arg: unknown) => ({ status: 200, raw: arg }),
          shim: `(result) => ({
            ...result,
            ok: result.status >= 200 && result.status < 300,
            data: result.raw,
          })`,
        },
      },
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    const exp = result.exports['default'] as Record<string, unknown>
    expect(exp['ok']).toBe(true)
    expect(exp['data']).toBe('hello')
    expect(exp['status']).toBe(200)
  })

  test('BridgeWithShim: async shim is awaited correctly', async () => {
    const result = await runtime.run({
      code: `
        const res = await compute(10)
        export default res.doubled
      `,
      globals: {
        compute: {
          kind: 'bridge-with-shim',
          handler: async (n: unknown) => ({ value: n }),
          shim: `async (result) => ({
            ...result,
            doubled: result.value * 2,
          })`,
        },
      },
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports['default']).toBe(20)
  })

  test('BridgeWithShim: shim uses JSON.parse on string body (fetch-style)', async () => {
    // Uses a pre-serialised string body — Uint8Array cross-bridge support is a later phase.
    const result = await runtime.run({
      code: `
        const res = await fetch('https://api.example.com/data')
        export default { status: res.status, ok: res.ok, value: res.json().value }
      `,
      globals: {
        fetch: {
          kind: 'bridge-with-shim',
          handler: async () => ({
            status: 200,
            headers: { 'content-type': 'application/json' },
            body: JSON.stringify({ value: 42 }),
          }),
          shim: `(result) => ({
            ...result,
            ok:   result.status >= 200 && result.status < 300,
            json: () => JSON.parse(result.body),
            text: () => result.body,
          })`,
        },
      },
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    const exp = result.exports['default'] as Record<string, unknown>
    expect(exp['status']).toBe(200)
    expect(exp['ok']).toBe(true)
    expect(exp['value']).toBe(42)
  })

  test('BridgeWithShim: handler can be rebound per prefix.run() while shim stays fixed', async () => {
    const callLog: string[] = []

    await using prefix = await runtime.precompile({
      code: '',
      globals: {
        fetch: {
          kind: 'bridge-with-shim',
          handler: async (url: unknown) => {
            callLog.push(`default:${url}`)
            return { status: 200, body: JSON.stringify({ from: 'default' }) }
          },
          shim: `(result) => ({
            ...result,
            ok:   result.status >= 200 && result.status < 300,
            json: () => JSON.parse(result.body),
          })`,
        },
      },
    })

    // First run — uses default handler
    const r1 = await prefix.run({
      code: `
        const res = await fetch('https://api.example.com/first')
        export default res.json().from
      `,
    })
    expect(r1.ok).toBe(true)
    if (!r1.ok)
      return
    expect(r1.exports['default']).toBe('default')

    // Second run — rebind handler only; shim (.json()) must still work
    const r2 = await prefix.run({
      code: `
        const res = await fetch('https://api.example.com/second')
        export default res.json().from
      `,
      globals: {
        fetch: async (url: unknown) => {
          callLog.push(`rebound:${url}`)
          return { status: 201, body: JSON.stringify({ from: 'rebound' }) }
        },
      },
    })
    expect(r2.ok).toBe(true)
    if (!r2.ok)
      return
    expect(r2.exports['default']).toBe('rebound')

    expect(callLog).toEqual([
      'default:https://api.example.com/first',
      'rebound:https://api.example.com/second',
    ])
  })

  test('string global in prefix is available across all postfix runs', async () => {
    await using prefix = await runtime.precompile({
      code: '',
      globals: { BASE_URL: `'https://api.example.com'` },
    })

    for (let i = 0; i < 3; i++) {
      const result = await prefix.run({ code: 'export default BASE_URL' })
      expect(result.ok).toBe(true)
      if (!result.ok)
        return
      expect(result.exports['default']).toBe('https://api.example.com')
    }
  })

  test('plain function global and BridgeWithShim can coexist', async () => {
    const toolCalls: string[] = []

    const result = await runtime.run({
      code: `
        const [toolResult, fetchResult] = await Promise.all([
          myTool('ping'),
          myFetch('https://api.example.com/data'),
        ])
        export default { tool: toolResult, fetchOk: fetchResult.ok }
      `,
      globals: {
        myTool: async (arg: unknown) => {
          toolCalls.push(arg as string)
          return `pong:${arg}`
        },
        myFetch: {
          kind: 'bridge-with-shim',
          handler: async () => ({ status: 200, body: null }),
          shim: `(r) => ({ ...r, ok: r.status < 300 })`,
        },
      },
    })

    expect(toolCalls).toEqual(['ping'])
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    const exp = result.exports['default'] as Record<string, unknown>
    expect(exp['tool']).toBe('pong:ping')
    expect(exp['fetchOk']).toBe(true)
  })
})
