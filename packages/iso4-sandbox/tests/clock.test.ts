/**
 * Timing posture pins (DESIGN.md §1.2): the sandbox clock is frozen while
 * guest code executes and advances only when the runtime regains control at
 * an event — run entry, bridge responses, stream frames (to real wall
 * time), and native timer fires (to the timer's SCHEDULED time only, #79).
 * All guest-visible clocks (Date, no-arg Intl.DateTimeFormat formatting,
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

describe('frozen clock — native timers (#79)', () => {
  // Timers virtualize onto the frozen clock, the workerd model: the delay
  // is scheduled FROM the frozen value and the fire advances the clock TO
  // exactly the scheduled value. A timer therefore never reveals real
  // elapsed wall time — the pins below are exact equalities on purpose.

  test('Date.now() advances by exactly the requested delay across a sleep', async () => {
    const delta = await evalDefault(`
      const a = Date.now()
      await new Promise(r => setTimeout(r, 120))
      export default Date.now() - a
    `)
    expect(delta).toBe(120)
  })

  test('a zero-delay timer after a CPU burn reveals no elapsed time', async () => {
    // The setTimeout(0) probe: the burn is real wall time the freeze hides,
    // and the timer fire must keep hiding it.
    const delta = await evalDefault(`
      const a = Date.now()
      let x = 0
      for (let i = 0; i < 5e6; i++) x = (x + i) % 97
      await new Promise(r => setTimeout(r, 0))
      export default Date.now() - a
    `)
    expect(delta).toBe(0)
  })

  test('chained sleeps accumulate exactly and stay monotone', async () => {
    const out = await evalDefault(`
      const a = Date.now()
      await new Promise(r => setTimeout(r, 30))
      const b = Date.now()
      await new Promise(r => setTimeout(r, 40))
      const c = Date.now()
      export default { first: b - a, second: c - b }
    `)
    expect(out).toEqual({ first: 30, second: 40 })
  })

  test('the clock stays constant within a timer callback turn', async () => {
    const ok = await evalDefault(`
      const inside = await new Promise(r => setTimeout(() => {
        const t1 = Date.now()
        let x = 0
        for (let i = 0; i < 2e6; i++) x += i
        r(Date.now() === t1)
      }, 10))
      export default inside
    `)
    expect(ok).toBe(true)
  })

  test('a bridge advance past a timer\'s scheduled time cannot run the clock backwards', async () => {
    // The host response advances the clock to real wall; a timer scheduled
    // earlier (but fired later) must clamp to max(prev, scheduled).
    const ok = await evalDefault(`
      const stamps = [Date.now()]
      await new Promise(r => setTimeout(r, 5))
      stamps.push(Date.now())
      await new Promise(r => setTimeout(r, 5))
      stamps.push(Date.now())
      export default stamps[1] >= stamps[0] && stamps[2] >= stamps[1]
    `)
    expect(ok).toBe(true)
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
