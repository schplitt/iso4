/**
 * Warm instance acceptance tests (#64).
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

describe('warm instances (#64)', () => {
  let single: Sandbox

  beforeAll(async () => {
    single = await createSandbox({ maxIsolates: 1 })
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

  test('LRU eviction: another prefix displaces the idle instance', async () => {
    // The live-isolate cap is budget-derived since #65; pin it to exactly
    // one instance (128 MB budget ÷ 128 MB cap) so warming B while A idles
    // must evict A's instance.
    await using tiny = await createSandbox({ maxIsolates: 1, memoryBudgetMb: 128 })
    await using prefixA = await tiny.prepare({ code: COUNTER })
    await using prefixB = await tiny.prepare({ code: COUNTER })

    const a1 = await prefixA.call({ export: 'bump' })
    expect(a1.ok && a1.value === 1).toBe(true)

    const b1 = await prefixB.call({ export: 'bump' })
    expect(b1.ok && b1.value === 1).toBe(true)

    // A was evicted for B — its next call cold-starts from n = 0.
    const a2 = await prefixA.call({ export: 'bump' })
    expect(a2.ok).toBe(true)
    if (!a2.ok)
      return
    expect(a2.value).toBe(1)
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

  test('an aborted call taints: the next call cold-starts clean', async () => {
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

    // terminate_execution ripped the call mid-flight — instance evicted.
    const fresh = await prefix.call({ export: 'bump' })
    expect(fresh.ok).toBe(true)
    if (!fresh.ok)
      return
    expect(fresh.value).toBe(1)
  })
})

describe('warm instances: concurrency (#64)', () => {
  let dual: Sandbox

  beforeAll(async () => {
    dual = await createSandbox({ maxIsolates: 2 })
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
    // instances-across-machines contract). Each call busy-waits so the two
    // must overlap; each sees its own fresh n.
    await using prefix = await dual.prepare({
      code: `let n = 0
export function slowBump() {
  const t = Date.now()
  while (Date.now() - t < 200) { /* hold the instance busy */ }
  return ++n
}`,
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
