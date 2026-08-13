/**
 * Capacity manager acceptance tests (#65).
 *
 * Two independent resources, two knobs: `maxIsolates` caps concurrent runs
 * (the connection pool, unchanged), `memoryBudgetMb` caps live isolates —
 * running plus kept-warm — at `budget ÷ memoryMb`, floored at the pool size.
 * Saturation always queues FIFO (no policy knobs — the wait is bounded by
 * the running calls' own limits), and `stats()` reports the registry over a
 * dedicated control connection that never queues behind runs.
 */

import process from 'node:process'
import { describe, expect, test, vi } from 'vitest'
import { createSandbox } from '../src/index.js'

const COUNTER = 'let n = 0\nexport function bump() { return ++n }'

async function sleep(ms: number): Promise<void> {
  await new Promise((resolve) => {
    setTimeout(resolve, ms)
  })
}

/**
 * Holds its instance busy for `ms` so concurrent calls must overlap.
 */
const SLOW = `export function slow(ms) {
  const t = Date.now()
  while (Date.now() - t < ms) { /* hold the instance busy */ }
  return 1
}`

describe('memory budget → live-isolate cap (#65)', () => {
  test('idle instances outlive the run-slot count under the default budget', async () => {
    // One run slot, but the budget-derived live cap (machine memory minus
    // the safety net) keeps BOTH prefixes resident — the #64 model would
    // have evicted A to warm B.
    await using single = await createSandbox({ maxIsolates: 1 })
    await using prefixA = await single.prepare({ code: COUNTER })
    await using prefixB = await single.prepare({ code: COUNTER })

    const a1 = await prefixA.call({ export: 'bump' })
    expect(a1.ok && a1.value === 1).toBe(true)
    const b1 = await prefixB.call({ export: 'bump' })
    expect(b1.ok && b1.value === 1).toBe(true)

    // A is still warm: its counter advances instead of resetting.
    const a2 = await prefixA.call({ export: 'bump' })
    expect(a2.ok).toBe(true)
    if (!a2.ok)
      return
    expect(a2.value).toBe(2)
  })

  test('an explicit memoryBudgetMb bounds resident instances', async () => {
    // 256 MB budget ÷ 128 MB cap = 2 live isolates: warming a third prefix
    // evicts the least-recently-used idle one.
    await using sandbox = await createSandbox({ maxIsolates: 1, memoryBudgetMb: 256 })
    await using prefixA = await sandbox.prepare({ code: COUNTER })
    await using prefixB = await sandbox.prepare({ code: COUNTER })
    await using prefixC = await sandbox.prepare({ code: COUNTER })

    for (const prefix of [prefixA, prefixB, prefixC]) {
      const first = await prefix.call({ export: 'bump' })
      expect(first.ok && first.value === 1).toBe(true)
    }

    // C displaced A (LRU); B survived within the budget.
    const b2 = await prefixB.call({ export: 'bump' })
    expect(b2.ok).toBe(true)
    if (b2.ok)
      expect(b2.value).toBe(2)
    const a2 = await prefixA.call({ export: 'bump' })
    expect(a2.ok).toBe(true)
    if (a2.ok)
      expect(a2.value).toBe(1)
  })

  test('memoryBudgetMb with uncapped isolates is rejected', async () => {
    await expect(
      createSandbox({ memoryMb: 0, memoryBudgetMb: 1024 }),
    ).rejects.toThrow(/memoryBudgetMb requires a nonzero memoryMb/)
  })

  test('the default budget respects a container memory constraint', async () => {
    // Pretend the process runs in a 2 GB container: constrainedMemory()
    // reports the cgroup limit os.totalmem() cannot see. Budget = 2048 minus
    // the max(512 MB, 25 %) safety net = 1536 MB → 12 live isolates at the
    // 128 MB default cap.
    const constrained = vi
      .spyOn(process, 'constrainedMemory')
      .mockReturnValue(2 * 1024 * 1024 * 1024)
    try {
      await using sandbox = await createSandbox({ maxIsolates: 2 })
      const stats = await sandbox.stats()
      expect(stats.maxLiveIsolates).toBe(12)
    } finally {
      constrained.mockRestore()
    }
  })
})

describe('saturation queues FIFO (#65)', () => {
  test('a queued call waits for the busy slot and then runs', async () => {
    // No queue knobs, deliberately: saturation always queues, and the wait
    // is bounded in practice because every run has wall/CPU limits.
    await using single = await createSandbox({ maxIsolates: 1 })
    await using prefix = await single.prepare({ code: SLOW })

    const busy = prefix.call({
      export: 'slow',
      args: [300],
      limits: { cpuTimeMs: 5_000 },
    })
    // Give the busy call a head start so it owns the only slot.
    await sleep(50)

    const queued = await prefix.call({ export: 'slow', args: [0] })
    expect(queued.ok).toBe(true)

    const first = await busy
    expect(first.ok).toBe(true)
  })
})

describe('stats() (#65)', () => {
  test('reports idle warm instances and their measured heap', async () => {
    await using sandbox = await createSandbox({ maxIsolates: 2 })
    await using prefix = await sandbox.prepare({ code: COUNTER })
    const warm = await prefix.call({ export: 'bump' })
    expect(warm.ok).toBe(true)

    const stats = await sandbox.stats()
    expect(stats.activeRuns).toBe(0)
    expect(stats.queueDepth).toBe(0)
    expect(stats.warmInstances).toBe(1)
    expect(stats.idleInstances).toBe(1)
    expect(stats.idleHeapBytes).toBeGreaterThan(0)
    expect(stats.maxLiveIsolates).toBeGreaterThanOrEqual(2)
    expect(stats.prefixes[prefix.id]).toEqual({ idle: 1, busy: 0 })
  })

  test('answers during saturation and sees the busy instance', async () => {
    // The control connection is not a pool slot: with the only slot held by
    // a running call, stats() must still answer — that is its whole point.
    await using single = await createSandbox({ maxIsolates: 1 })
    await using prefix = await single.prepare({ code: SLOW })

    const busy = prefix.call({
      export: 'slow',
      args: [500],
      limits: { cpuTimeMs: 5_000 },
    })
    await sleep(100)

    const stats = await single.stats()
    expect(stats.activeRuns).toBe(1)
    expect(stats.prefixes[prefix.id]?.busy).toBe(1)

    const first = await busy
    expect(first.ok).toBe(true)
  })

  test('counts a running one-off in activeRuns', async () => {
    await using dual = await createSandbox({ maxIsolates: 2 })
    const busy = dual.run({
      code: `const t = Date.now()
while (Date.now() - t < 500) { /* spin */ }
export default 1`,
      limits: { cpuTimeMs: 5_000 },
    })
    await sleep(100)

    const stats = await dual.stats()
    expect(stats.activeRuns).toBe(1)
    expect(stats.warmInstances).toBe(0)

    const done = await busy
    expect(done.ok).toBe(true)
  })
})
