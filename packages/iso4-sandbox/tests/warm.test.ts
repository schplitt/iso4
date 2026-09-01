/**
 * Warm instance acceptance tests.
 *
 * Every prepared prefix is served by warm instances: the first run
 * cold-starts an isolate (prefix evaluated under the fixed warm-up budget),
 * later runs reuse it. The contract is workerd's: warmth is a cache, never a
 * guarantee — state carryover between calls is permitted, unguaranteed, and
 * vanishes on eviction (taint, LRU, dispose). One-off `sandbox.run()` is
 * untouched: always a fresh isolate.
 *
 * Single-slot sandboxes make reuse and eviction deterministic; the
 * concurrency tests use two slots.
 */

import { afterAll, beforeAll, describe, expect, test } from 'vitest'
import { createSandbox } from '../src/index.js'
import type { Sandbox } from '../src/index.js'

const COUNTER = 'let n = 0\nexport function bump() { return ++n }'

describe('warm instances', () => {
  let single: Sandbox

  beforeAll(async () => {
    single = await createSandbox({ maxConcurrentRuns: 1 })
  })

  afterAll(async () => {
    await single.dispose()
  })

  test('module-scope state survives across calls on one instance', async () => {
    await using prefix = await single.prepare({ code: COUNTER })
    for (const expected of [1, 2, 3]) {
      const result = await prefix.call({ export: 'bump' })
      expect(result.ok).toBe(true)
      if (!result.ok)
        return
      expect(result.value).toBe(expected)
    }
  })

  test('a limit violation evicts: the next call cold-starts clean', async () => {
    await using prefix = await single.prepare({
      code: `${COUNTER}\nexport function spin() { for (;;) {} }`,
    })
    const warm = await prefix.call({ export: 'bump' })
    expect(warm.ok).toBe(true)

    const blown = await prefix.call({
      export: 'spin',
      limits: { cpuTimeMs: 100, wallTimeMs: 5_000 },
    })
    expect(blown.ok).toBe(false)
    if (blown.ok)
      return
    expect(['ERR_CPU_TIMEOUT', 'ERR_WALL_TIMEOUT']).toContain(blown.error.code)

    // The tainted instance was discarded — a fresh one starts from n = 0.
    const fresh = await prefix.call({ export: 'bump' })
    expect(fresh.ok).toBe(true)
    if (!fresh.ok)
      return
    expect(fresh.value).toBe(1)
  })

  test('no count cap: warming another prefix displaces nothing', async () => {
    // The earlier count-based eviction is gone — celld's stance (their
    // resident ceiling defaults to unlimited after a default count cap
    // caused eviction churn). Even with a single run slot and watermarks
    // explicitly disabled, warming B leaves A resident; only RSS pressure
    // (capacity.test.ts) or dispose() evicts.
    await using tiny = await createSandbox({ maxConcurrentRuns: 1, memoryBudgetMb: 0 })
    await using prefixA = await tiny.prepare({ code: COUNTER })
    await using prefixB = await tiny.prepare({ code: COUNTER })

    const a1 = await prefixA.call({ export: 'bump' })
    expect(a1.ok && a1.value === 1).toBe(true)

    const b1 = await prefixB.call({ export: 'bump' })
    expect(b1.ok && b1.value === 1).toBe(true)

    // A is still warm — its counter advances instead of resetting.
    const a2 = await prefixA.call({ export: 'bump' })
    expect(a2.ok).toBe(true)
    if (!a2.ok)
      return
    expect(a2.value).toBe(2)
  })

  test('heapUsedBytes is reported for prefix runs, absent for one-off runs', async () => {
    await using prefix = await single.prepare({ code: COUNTER })
    const call = await prefix.call({ export: 'bump' })
    expect(call.ok).toBe(true)
    expect(call.heapUsedBytes).toBeGreaterThan(0)

    const execute = await prefix.execute({ code: 'export default 1' })
    expect(execute.ok).toBe(true)
    expect(execute.heapUsedBytes).toBeGreaterThan(0)

    const oneOff = await single.run({ code: 'export default 1' })
    expect(oneOff.ok).toBe(true)
    expect(oneOff.heapUsedBytes).toBeUndefined()
  })

  test('prepare() rejects a prefix that blows the warm-up budget', async () => {
    // A synchronously looping prefix used to hang prepare() forever; the
    // fixed warm-up budget (1s, Cloudflare's script-startup model) now
    // rejects it at deploy time with its own code.
    await expect(
      single.prepare({ code: 'for (;;) {}' }),
    ).rejects.toMatchObject({ code: 'ERR_WARMUP_LIMIT' })
  }, 15_000)

  test('one-off runs never share state', async () => {
    const first = await single.run({
      code: 'globalThis.__mark = 1; export default 1',
    })
    expect(first.ok).toBe(true)
    const second = await single.run({
      code: 'export default globalThis.__mark ?? "fresh"',
    })
    expect(second.ok).toBe(true)
    if (!second.ok)
      return
    expect(second.exports.default).toBe('fresh')
  })

  test('a wall timeout while suspended keeps the instance warm', async () => {
    // The run is parked awaiting a bridge response when its wall budget runs
    // out: a boundary failure, not a mid-execution kill — the instance and
    // its state survive (this used to evict).
    await using prefix = await single.prepare({
      code: `${COUNTER}\nexport async function hangOnBridge() { n++; await hang(); return n }`,
      globals: { hang: async () => new Promise(() => {}) },
    })
    const warm = await prefix.call({ export: 'bump' })
    expect(warm.ok && warm.value === 1).toBe(true)

    const timedOut = await prefix.call({
      export: 'hangOnBridge',
      limits: { wallTimeMs: 300 },
    })
    expect(timedOut.ok).toBe(false)
    if (timedOut.ok)
      return
    expect(timedOut.error.code).toBe('ERR_WALL_TIMEOUT')

    // Warm reuse with state intact: hangOnBridge's n++ is visible.
    const fresh = await prefix.call({ export: 'bump' })
    expect(fresh.ok).toBe(true)
    if (!fresh.ok)
      return
    expect(fresh.value).toBe(3)
  })

  test('aborting a CPU-bound run kills it mid-execution with real telemetry', async () => {
    // A synchronous spin never reads the socket, so the Terminate frame
    // alone could not stop it — the runtime now terminates the executing
    // turn directly. The proof it was the graceful path and not the old
    // connection-teardown fallback: the result carries REAL telemetry
    // (the teardown fallback synthesizes zeros).
    const controller = new AbortController()
    const pending = single.run({
      code: 'for (;;) {}',
      signal: controller.signal,
      limits: { cpuTimeMs: 30_000, wallTimeMs: 30_000 },
    })
    setTimeout(() => controller.abort('cancelled'), 200)
    const started = Date.now()
    const aborted = await pending
    expect(aborted.status).toBe('aborted')
    expect(Date.now() - started).toBeLessThan(5_000)
    if (aborted.ok)
      return
    expect(aborted.durationMs).toBeGreaterThan(0)

    // The sandbox keeps serving afterwards.
    const after = await single.run({ code: 'export default 1' })
    expect(after.ok).toBe(true)
  })

  test('an abort while suspended abandons the run; the instance survives', async () => {
    // The run is parked awaiting a bridge response when the abort lands, so
    // nothing is interrupted mid-execution: the run is simply abandoned (its
    // continuations never run again) and the instance stays trustworthy and
    // warm. Only an abort landing on actively-executing code still
    // terminates and evicts.
    await using prefix = await single.prepare({
      code: `${COUNTER}\nexport async function hangOnBridge() { n++; await hang(); return n }`,
      globals: { hang: async () => new Promise(() => {}) },
    })
    const warm = await prefix.call({ export: 'bump' })
    expect(warm.ok && warm.value === 1).toBe(true)

    const controller = new AbortController()
    const pending = prefix.call({
      export: 'hangOnBridge',
      signal: controller.signal,
      limits: { wallTimeMs: 10_000 },
    })
    setTimeout(() => controller.abort('cancelled'), 150)
    const aborted = await pending
    expect(aborted.status).toBe('aborted')

    // Warm reuse: the abandoned run's completed side effect (n++ before the
    // hang) is visible, exactly like any other state a warm instance keeps.
    const fresh = await prefix.call({ export: 'bump' })
    expect(fresh.ok).toBe(true)
    if (!fresh.ok)
      return
    expect(fresh.value).toBe(3)
  })
})

describe('warm instances: concurrency', () => {
  let dual: Sandbox

  beforeAll(async () => {
    dual = await createSandbox({ maxConcurrentRuns: 2 })
  })

  afterAll(async () => {
    await dual.dispose()
  })

  test('two prefixes stay warm concurrently without interference', async () => {
    await using prefixA = await dual.prepare({ code: COUNTER })
    await using prefixB = await dual.prepare({ code: COUNTER })

    for (const expected of [1, 2]) {
      const [a, b] = await Promise.all([
        prefixA.call({ export: 'bump' }),
        prefixB.call({ export: 'bump' }),
      ])
      expect(a.ok && a.value === expected).toBe(true)
      expect(b.ok && b.value === expected).toBe(true)
    }
  })

  test('concurrent calls on one prefix get separate instances', async () => {
    // v1 concurrency: one call at a time per instance — parallelism for one
    // prefix means more instances, which share no state (the workerd
    // instances-across-machines contract). Each call parks on a host sleep
    // so the two must overlap (the sandbox clock is frozen during execution,
    // so a spin-wait would never terminate); each sees its own fresh n.
    await using prefix = await dual.prepare({
      code: `let n = 0
export async function slowBump() {
  await hold()
  return ++n
}`,
      globals: {
        hold: async () =>
          new Promise((resolve) => {
            setTimeout(resolve, 200)
          }),
      },
    })
    const [first, second] = await Promise.all([
      prefix.call({ export: 'slowBump', limits: { cpuTimeMs: 5_000 } }),
      prefix.call({ export: 'slowBump', limits: { cpuTimeMs: 5_000 } }),
    ])
    expect(first.ok).toBe(true)
    expect(second.ok).toBe(true)
    if (!first.ok || !second.ok)
      return
    expect(first.value).toBe(1)
    expect(second.value).toBe(1)
  })
})
