/**
 * Capacity manager acceptance tests.
 *
 * Two independent resources, two knobs: `maxIsolates` caps concurrent runs
 * (the connection pool, unchanged), `memoryBudgetMb` is the ONE memory
 * mark — the runtime watches its own process RSS against it; at/above the
 * mark it evicts idle instances by `heapUsed × idleTime` score AND stops
 * pooling new ones (prefix runs degrade to cold one-off isolates) until
 * RSS falls back to 80 % of the mark. There is no instance-count cap.
 * Saturation always queues FIFO (no policy knobs — the wait is bounded by
 * the running calls' own limits), and `stats()` reports the registry over
 * a dedicated control connection that never queues behind runs.
 */

import { totalmem } from 'node:os'
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

describe('memory budget → live-isolate cap', () => {
  test('idle instances outlive the run-slot count under the default budget', async () => {
    // One run slot, but the budget-derived live cap (machine memory minus
    // the safety net) keeps BOTH prefixes resident — the old slot model would
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

  test('a healthy explicit budget keeps every prefix resident', async () => {
    // Memory, not instance count, decides how much stays warm: with RSS
    // far below the soft mark of a generous budget, warming a third prefix
    // evicts nothing — under the old count model (256 ÷ 128 = 2 slots) prefix
    // C would have displaced A here.
    await using sandbox = await createSandbox({ maxIsolates: 1, memoryBudgetMb: 4096 })
    await using prefixA = await sandbox.prepare({ code: COUNTER })
    await using prefixB = await sandbox.prepare({ code: COUNTER })
    await using prefixC = await sandbox.prepare({ code: COUNTER })

    for (const prefix of [prefixA, prefixB, prefixC]) {
      const first = await prefix.call({ export: 'bump' })
      expect(first.ok && first.value === 1).toBe(true)
    }

    // All three counters advance — nothing was evicted.
    for (const prefix of [prefixA, prefixB, prefixC]) {
      const second = await prefix.call({ export: 'bump' })
      expect(second.ok).toBe(true)
      if (second.ok)
        expect(second.value).toBe(2)
    }

    const stats = await sandbox.stats()
    expect(stats.underPressure).toBe(false)
  })

  test('hard memory pressure degrades prefix runs to cold isolates', async () => {
    // A budget far below the process's real footprint pins RSS over the
    // hard mark from the first sample: nothing may be pooled, every prefix
    // run gets a fresh cold isolate — calls still succeed, state resets
    // between calls, and nothing sits idle. Correctness never depends on
    // warmth. (Do NOT assert RSS drops — freed heap returns to the OS
    // lazily; that is what the futility check is for.)
    await using pressured = await createSandbox({ maxIsolates: 1, memoryBudgetMb: 8 })
    await using prefix = await pressured.prepare({ code: COUNTER })

    const first = await prefix.call({ export: 'bump' })
    expect(first.ok).toBe(true)
    if (first.ok)
      expect(first.value).toBe(1)

    // A warm instance would answer 2 here; a cold one answers 1 again.
    const second = await prefix.call({ export: 'bump' })
    expect(second.ok).toBe(true)
    if (second.ok)
      expect(second.value).toBe(1)

    const stats = await pressured.stats()
    expect(stats.underPressure).toBe(true)
    expect(stats.idleInstances).toBe(0)
    expect(stats.warmInstances).toBe(0)
    expect(stats.rssBytes).toBeGreaterThan(8 * 1024 * 1024)
  })

  test('memoryBudgetMb works with uncapped isolates', async () => {
    // Under the count model this combination threw (no per-isolate cap to
    // divide by). RSS is measured, not derived, so the budget now applies
    // regardless of memoryMb.
    await using sandbox = await createSandbox({ memoryMb: 0, memoryBudgetMb: 4096 })
    const stats = await sandbox.stats()
    expect(stats.budgetBytes).toBe(4096 * 1024 * 1024)
  })

  test('a non-finite memoryBudgetMb is rejected instead of killing the child', async () => {
    // Infinity ("unlimited") would otherwise reach the child as
    // `--max-live-isolates Infinity` and surface as a socket timeout.
    await expect(
      createSandbox({ memoryBudgetMb: Number.POSITIVE_INFINITY }),
    ).rejects.toThrow(/memoryBudgetMb must be a finite number/)
    await expect(
      createSandbox({ memoryBudgetMb: Number.NaN }),
    ).rejects.toThrow(/memoryBudgetMb must be a finite number/)
  })

  test('a small-memory host floors the default budget instead of disabling it', async () => {
    // On a host at or below the 512 MB safety net the derived default goes
    // to zero or negative — which must NOT silently disable the watermarks
    // on exactly the machines that need them (the count cap that used to
    // backstop this case is gone). The default floors at 64 MB; explicit
    // memoryBudgetMb: 0 stays the only opt-out.
    const constrained = vi
      .spyOn(process, 'constrainedMemory')
      .mockReturnValue(256 * 1024 * 1024)
    try {
      await using sandbox = await createSandbox({ maxIsolates: 1 })
      const stats = await sandbox.stats()
      expect(stats.budgetBytes).toBe(64 * 1024 * 1024)
    } finally {
      constrained.mockRestore()
    }
  })

  test('a huge finite budget is clamped, not passed through broken', async () => {
    // 1e15 MB in bytes exceeds 2^53 (silent rounding) and 1e21 stringifies
    // to exponential notation, which would kill the child at arg parsing
    // and surface as a socket timeout. The clamp keeps the spawn alive and
    // the mark at the JS safe-integer ceiling.
    await using sandbox = await createSandbox({ maxIsolates: 1, memoryBudgetMb: 1e15 })
    const stats = await sandbox.stats()
    expect(stats.budgetBytes).toBe(Number.MAX_SAFE_INTEGER)
  })

  test('the cgroup-v1 "unlimited" sentinel falls back to host memory', async () => {
    // On cgroup v1 (GitHub Actions runners), constrainedMemory() reports
    // "no limit" as a sentinel near 2^63 instead of 0. Trusting it produced
    // a live cap that was a multiple of 2^32 — which the wire's u32
    // truncated to exactly 0. The default must clamp to os.totalmem().
    const constrained = vi
      .spyOn(process, 'constrainedMemory')
      .mockReturnValue(2 ** 63)
    try {
      await using sandbox = await createSandbox({ maxIsolates: 2 })
      const stats = await sandbox.stats()
      const totalMb = totalmem() / (1024 * 1024)
      // The default budget must be clamped to the host total, not the
      // sentinel; stats reports it in bytes.
      const expected = Math.floor(totalMb - Math.max(512, totalMb * 0.25)) * 1024 * 1024
      expect(stats.budgetBytes).toBe(expected)
    } finally {
      constrained.mockRestore()
    }
  })

  test('the default budget respects a container memory constraint', async () => {
    // Pretend the process runs in a 2 GB container: constrainedMemory()
    // reports the cgroup limit os.totalmem() cannot see. Budget = 2048 minus
    // the max(512 MB, 25 %) safety net = 1536 MB — the RSS mark the runtime
    // sheds against.
    const constrained = vi
      .spyOn(process, 'constrainedMemory')
      .mockReturnValue(2 * 1024 * 1024 * 1024)
    try {
      await using sandbox = await createSandbox({ maxIsolates: 2 })
      const stats = await sandbox.stats()
      expect(stats.budgetBytes).toBe(1536 * 1024 * 1024)
    } finally {
      constrained.mockRestore()
    }
  })
})

describe('saturation queues FIFO', () => {
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

describe('stats()', () => {
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
    // The watermark signal is visible — the mark, a real RSS reading,
    // and no pressure with a healthy default budget on an idle sandbox.
    expect(stats.budgetBytes).toBeGreaterThan(0)
    expect(stats.rssBytes).toBeGreaterThan(1024 * 1024)
    expect(stats.underPressure).toBe(false)
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
