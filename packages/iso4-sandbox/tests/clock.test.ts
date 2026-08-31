/**
 * Timing posture pins (DESIGN.md §1.2): the sandbox clock is frozen while
 * guest code executes and advances only when the runtime regains control at
 * a socket frame — run entry, bridge responses, stream frames. All
 * guest-visible clocks (Date, no-arg Intl.DateTimeFormat formatting,
 * Temporal.Now) read the same frozen value; SharedArrayBuffer is removed and
 * Atomics.wait is disabled, so no replacement timer can be built.
 *
 * These are behavioral pins, not feature tests: a failure here means the
 * timing posture regressed, which is a security property.
 */

import type { Sandbox } from '../src/index'
import { afterAll, beforeAll, describe, expect, test } from 'vitest'
import { createSandbox } from '../src/index'

let runtime: Sandbox

beforeAll(async () => {
  runtime = await createSandbox()
})

afterAll(async () => {
  await runtime?.dispose()
})

// Run a snippet and return its default export (fails the test on error).
async function evalDefault(code: string): Promise<unknown> {
  const result = await runtime.run({ code })
  expect(result.ok, result.ok ? undefined : JSON.stringify(result.error)).toBe(true)
  if (!result.ok)
    throw new Error('unreachable')
  return result.exports.default
}

describe('frozen clock — Date', () => {
  test('Date.now() does not advance across a CPU-bound loop', async () => {
    const delta = await evalDefault(`
      const a = Date.now()
      let x = 0
      for (let i = 0; i < 5e6; i++) x += i
      export default Date.now() - a
    `)
    expect(delta).toBe(0)
  })

  test('Date.now() does not advance across awaited microtask chains', async () => {
    const delta = await evalDefault(`
      const a = Date.now()
      for (let i = 0; i < 1000; i++) await Promise.resolve()
      export default Date.now() - a
    `)
    expect(delta).toBe(0)
  })

  test('no-arg new Date(), Date() and Date.now() agree', async () => {
    const ok = await evalDefault(`
      export default new Date().getTime() === Date.now()
        && Date() === new Date(Date.now()).toString()
    `)
    expect(ok).toBe(true)
  })

  test('the clock advances across a bridge call and stays monotone', async () => {
    const result = await runtime.run({
      globals: {
        hostSleep: async () => {
          await new Promise<void>((resolve) => {
            setTimeout(resolve, 20)
          })
          return 1
        },
      },
      code: `
        const stamps = [Date.now()]
        await hostSleep()
        stamps.push(Date.now())
        await hostSleep()
        stamps.push(Date.now())
        export default {
          firstAdvance: stamps[1] - stamps[0],
          monotone: stamps[0] <= stamps[1] && stamps[1] <= stamps[2],
        }
      `,
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    const out = result.exports.default as { firstAdvance: number, monotone: boolean }
    expect(out.firstAdvance).toBeGreaterThanOrEqual(20)
    expect(out.monotone).toBe(true)
  })

  test('the clock advances between runs on the same runtime', async () => {
    const first = await evalDefault('export default Date.now()') as number
    await new Promise<void>((resolve) => {
      setTimeout(resolve, 25)
    })
    const second = await evalDefault('export default Date.now()') as number
    expect(second - first).toBeGreaterThanOrEqual(20)
  })

  test('explicit-argument Date construction and statics pass through', async () => {
    const ok = await evalDefault(`
      class D extends Date {}
      export default new Date(1234).getTime() === 1234
        && new D(1234) instanceof Date
        && new D(1234).getTime() === 1234
        && Date.parse('2020-01-01T00:00:00Z') === 1577836800000
        && Date.UTC(2020, 0, 1) === 1577836800000
        && new Date(1234).constructor === Date
    `)
    expect(ok).toBe(true)
  })

  test('the shim is shaped like the built-in (name, whole-ms, no enumerable statics)', async () => {
    const ok = await evalDefault(`
      export default Date.name === 'Date'
        && Date.now.name === 'now'
        && Date.now() % 1 === 0
        && Object.keys(Date).length === 0
    `)
    expect(ok).toBe(true)
  })
})

describe('frozen clock — the other clock surfaces', () => {
  test('no-arg Intl.DateTimeFormat format/formatToParts use the frozen clock', async () => {
    const ok = await evalDefault(`
      const fmt = new Intl.DateTimeFormat('en', {
        hour: 'numeric', minute: 'numeric', second: 'numeric', fractionalSecondDigits: 3,
      })
      export default fmt.format() === fmt.format(Date.now())
        && JSON.stringify(fmt.formatToParts()) === JSON.stringify(fmt.formatToParts(Date.now()))
    `)
    expect(ok).toBe(true)
  })

  test('Temporal.Now reads the frozen clock', async () => {
    const ok = await evalDefault(`
      export default typeof Temporal === 'undefined'
        || (Temporal.Now.instant().epochMilliseconds === Date.now()
          && Temporal.Now.zonedDateTimeISO().epochMilliseconds === Date.now())
    `)
    expect(ok).toBe(true)
  })

  test('performance and console.time stay absent', async () => {
    const ok = await evalDefault(`
      export default typeof performance === 'undefined' && typeof console.time === 'undefined'
    `)
    expect(ok).toBe(true)
  })
})

describe('frozen clock — warm prefix path', () => {
  test('prefix.run() calls are frozen and advance per call', async () => {
    const prefix = await runtime.precompile({
      code: 'globalThis.bootTime = Date.now()',
    })
    const first = await prefix.run({
      code: `
        const a = Date.now()
        let x = 0
        for (let i = 0; i < 5e6; i++) x += i
        export default { delta: Date.now() - a, now: Date.now(), bootTime: globalThis.bootTime }
      `,
    })
    expect(first.ok).toBe(true)
    if (!first.ok)
      return
    const one = first.exports.default as { delta: number, now: number, bootTime: number }
    expect(one.delta).toBe(0)
    // A call starts at run entry time, at or after the prefix's warm-up time.
    expect(one.now).toBeGreaterThanOrEqual(one.bootTime)

    await new Promise<void>((resolve) => {
      setTimeout(resolve, 25)
    })
    const second = await prefix.run({ code: 'export default Date.now()' })
    expect(second.ok).toBe(true)
    if (!second.ok)
      return
    expect((second.exports.default as number) - one.now).toBeGreaterThanOrEqual(20)
    await prefix.dispose()
  })
})

describe('no replacement timers', () => {
  test('SharedArrayBuffer is not exposed', async () => {
    const ok = await evalDefault(`
      export default typeof SharedArrayBuffer === 'undefined'
        && !('SharedArrayBuffer' in globalThis)
    `)
    expect(ok).toBe(true)
  })

  test('Atomics stays usable on plain ArrayBuffers, but Atomics.wait throws', async () => {
    const out = await evalDefault(`
      const plain = Atomics.add(new Int32Array(new ArrayBuffer(8)), 0, 1)
      // Wasm shared memory can still mint a shared buffer; without a second
      // thread it is inert, and the one blocking primitive throws:
      let waitResult
      try {
        const mem = new WebAssembly.Memory({ initial: 1, maximum: 1, shared: true })
        Atomics.wait(new Int32Array(mem.buffer), 0, 0, 5)
        waitResult = 'did not throw'
      }
      catch (e) {
        waitResult = e instanceof TypeError ? 'TypeError' : String(e)
      }
      export default { plain, waitResult }
    `)
    expect(out).toEqual({ plain: 0, waitResult: 'TypeError' })
  })
})
