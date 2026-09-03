/**
 * Capacity manager acceptance tests.
 *
 * Two independent resources, two knobs: `maxConcurrentRuns` caps runs
 * executing at once (admission only — connections open on demand and are
 * reused), `memoryBudgetMb` is the ONE memory
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
 * Holds its instance busy for `ms` so concurrent calls must overlap. The
 * hold parks on a host bridge sleep — the sandbox clock is frozen during
 * execution, so an in-sandbox spin-wait would never terminate — and a call
 * awaiting a bridge response holds its slot exactly like a running one.
 * Prefixes prepared from this must declare `globals: { hostSleep }`.
 */
const SLOW = `export async function slow(ms) {
  await hostSleep(ms)
  return 1
}`

const hostSleep = (ms: number): Promise<void> => sleep(ms)

describe('memory budget → live-isolate cap', () => {
  test('idle instances outlive the run-slot count under the default budget', async () => {
    // One run slot, but the budget-derived live cap (machine memory minus
    // the safety net) keeps BOTH prefixes resident — the old slot model would
    // have evicted A to warm B.
    await using single = await createSandbox({ maxConcurrentRuns: 1 })
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
    await using sandbox = await createSandbox({ maxConcurrentRuns: 1, memoryBudgetMb: 4096 })
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
    await using pressured = await createSandbox({ maxConcurrentRuns: 1, memoryBudgetMb: 8 })
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
      await using sandbox = await createSandbox({ maxConcurrentRuns: 1 })
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
    await using sandbox = await createSandbox({ maxConcurrentRuns: 1, memoryBudgetMb: 1e15 })
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
      await using sandbox = await createSandbox({ maxConcurrentRuns: 2 })
      const stats = await sandbox.stats()
      const totalMb = totalmem() / (1024 * 1024)
      // The default budget must be clamped to the host total, not the
      // sentinel; stats reports it in bytes. 80% of (total − the 256 MB
      // host reserve).
      const expected = Math.floor((totalMb - 256) * 0.8) * 1024 * 1024
      expect(stats.budgetBytes).toBe(expected)
    } finally {
      constrained.mockRestore()
    }
  })

  test('the default budget respects a container memory constraint', async () => {
    // Pretend the process runs in a 2 GB container: constrainedMemory()
    // reports the cgroup limit os.totalmem() cannot see. Budget = 80% of
    // (2048 − the 256 MB host reserve) = 1433 MB — the shedding mark the
    // runtime holds against global container memory.
    const constrained = vi
      .spyOn(process, 'constrainedMemory')
      .mockReturnValue(2 * 1024 * 1024 * 1024)
    try {
      await using sandbox = await createSandbox({ maxConcurrentRuns: 2 })
      const stats = await sandbox.stats()
      expect(stats.budgetBytes).toBe(Math.floor((2048 - 256) * 0.8) * 1024 * 1024)
    } finally {
      constrained.mockRestore()
    }
  })
})

describe('saturation queues FIFO', () => {
  test('a queued call waits for the busy slot and then runs', async () => {
    // No queue knobs, deliberately: saturation always queues, and the wait
    // is bounded in practice because every run has wall/CPU limits.
    await using single = await createSandbox({ maxConcurrentRuns: 1 })
    await using prefix = await single.prepare({ code: SLOW, globals: { hostSleep } })

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

  test('past maxQueuedRuns a run is shed with ERR_QUEUE_FULL, not queued', async () => {
    // Queue bound 0: the one slot is busy, so the next caller is refused
    // immediately as a failed result — nothing ran, telemetry is zero.
    await using single = await createSandbox({ maxConcurrentRuns: 1, maxQueuedRuns: 0 })
    await using prefix = await single.prepare({ code: SLOW, globals: { hostSleep } })

    const busy = prefix.call({
      export: 'slow',
      args: [300],
      limits: { cpuTimeMs: 5_000 },
    })
    await sleep(50)

    const shed = await prefix.call({ export: 'slow', args: [0] })
    expect(shed.ok).toBe(false)
    if (!shed.ok) {
      expect(shed.status).toBe('failed')
      expect(shed.error.code).toBe('ERR_QUEUE_FULL')
      expect(shed.error.message).toContain('maxQueuedRuns')
      expect(shed.durationMs).toBe(0)
    }

    // The busy run is untouched, and a freed slot admits again.
    const first = await busy
    expect(first.ok).toBe(true)
    const after = await prefix.call({ export: 'slow', args: [0] })
    expect(after.ok).toBe(true)
  })

  test('maxQueuedRuns rejects a fractional or negative bound', async () => {
    await expect(createSandbox({ maxQueuedRuns: 1.5 })).rejects.toThrow(/maxQueuedRuns/)
    await expect(createSandbox({ maxQueuedRuns: -1 })).rejects.toThrow(/maxQueuedRuns/)
  })
})

describe('per-workload heap caps (#77)', () => {
  const HOARD = `export function hoard(mb) {
    const arrays = []
    for (let i = 0; i < mb; i++) arrays.push(new Uint8Array(1024 * 1024).fill(1))
    return arrays.length
  }`

  test('a per-prefix cap binds that prefix; siblings keep the default', async () => {
    await using sandbox = await createSandbox({ maxConcurrentRuns: 2 })
    await using tight = await sandbox.prepare({ code: HOARD, memoryMb: 32 })
    await using roomy = await sandbox.prepare({ code: HOARD })

    const blown = await tight.call({ export: 'hoard', args: [64] })
    expect(blown.ok).toBe(false)
    if (!blown.ok && blown.status === 'failed')
      expect(blown.error.code).toBe('ERR_MEMORY_LIMIT')

    // Same allocation under the sandbox default (128 MB): fine.
    const fits = await roomy.call({ export: 'hoard', args: [64] })
    expect(fits.ok).toBe(true)
  })

  test('a one-off run may cap its own fresh isolate', async () => {
    await using sandbox = await createSandbox({ maxConcurrentRuns: 1 })
    const blown = await sandbox.run({
      code: `const a = []; for (let i = 0; i < 64; i++) a.push(new Uint8Array(1024 * 1024).fill(1))`,
      limits: { memoryMb: 32, wallTimeMs: 10_000, cpuTimeMs: 10_000 },
    })
    expect(blown.ok).toBe(false)
    if (!blown.ok && blown.status === 'failed')
      expect(blown.error.code).toBe('ERR_MEMORY_LIMIT')

    const fits = await sandbox.run({
      code: `const a = []; for (let i = 0; i < 64; i++) a.push(new Uint8Array(1024 * 1024).fill(1)); export default a.length`,
      limits: { wallTimeMs: 10_000, cpuTimeMs: 10_000 },
    })
    expect(fits.ok).toBe(true)
  })

  test('prefix runs still reject a per-run memoryMb; bad caps throw', async () => {
    await using sandbox = await createSandbox({ maxConcurrentRuns: 1 })
    await using prefix = await sandbox.prepare({ code: COUNTER })
    await expect(async () =>
      prefix.call({ export: 'bump', limits: { memoryMb: 64 } as never }),
    ).rejects.toThrow(/prepare\(\{ memoryMb \}\)/)
    await expect(sandbox.prepare({ code: COUNTER, memoryMb: 1.5 })).rejects.toThrow(/memoryMb/)
    await expect(sandbox.run({ code: '1', limits: { memoryMb: -1 } })).rejects.toThrow(/memoryMb/)
  })
})

describe('prefix-aware acquire (#77)', () => {
  test('waiting-heavy concurrent runs share one instance', async () => {
    await using sandbox = await createSandbox({ maxConcurrentRuns: 4 })
    await using prefix = await sandbox.prepare({ code: SLOW, globals: { hostSleep } })

    // Seed the prefix's demand averages: one completed run whose CPU share
    // is tiny (it parks on the host sleep). Until this completes, concurrent
    // runs would spawn their own isolates (unmeasured = pre-#77 behavior).
    const seed = await prefix.call({ export: 'slow', args: [20] })
    expect(seed.ok).toBe(true)

    // Three concurrent waiting-heavy runs: measured CPU demand is far under
    // one thread (sub-ms CPU × tens of arrivals/s), so they must all JOIN
    // the one warm instance instead of opening three isolates.
    const batch = [1, 2, 3].map(() =>
      prefix.call({ export: 'slow', args: [400], limits: { cpuTimeMs: 5_000 } }))
    await sleep(150)
    const stats = await sandbox.stats()
    expect(stats.activeRuns).toBe(3)
    const perPrefix = Object.values(stats.prefixes)[0]
    expect(perPrefix?.busy).toBe(1)
    expect(stats.warmInstances).toBe(1)

    for (const result of await Promise.all(batch))
      expect(result.ok).toBe(true)
  })
})

describe('lazy connections', () => {
  test('connections open on demand and are kept for reuse', async () => {
    // createSandbox no longer opens one socket per capacity unit up front:
    // only the stats control connection exists at creation (not counted in
    // openConnections), so a fresh sandbox reports zero run connections
    // however large the admission cap is.
    await using sandbox = await createSandbox({ maxConcurrentRuns: 8 })
    expect((await sandbox.stats()).openConnections).toBe(0)

    const first = await sandbox.run({ code: 'export default 1' })
    expect(first.ok).toBe(true)
    expect((await sandbox.stats()).openConnections).toBe(1)

    // A second sequential run reuses the idle connection instead of
    // opening another.
    const second = await sandbox.run({ code: 'export default 2' })
    expect(second.ok).toBe(true)
    expect((await sandbox.stats()).openConnections).toBe(1)
  })

  test('concurrent runs multiplex on one connection until its run cap', async () => {
    // The #124 activation shape: frames route by run id, so concurrent runs
    // share a connection instead of opening one each — 4 overlapping calls
    // (the per-connection cap) ride the single connection prepare() opened.
    await using sandbox = await createSandbox({ maxConcurrentRuns: 8 })
    await using prefix = await sandbox.prepare({ code: SLOW, globals: { hostSleep } })

    const four = await Promise.all(Array.from({ length: 4 }, () =>
      prefix.call({ export: 'slow', args: [150], limits: { cpuTimeMs: 5_000 } })))
    for (const r of four)
      expect(r.ok).toBe(true)
    expect((await sandbox.stats()).openConnections).toBe(1)

    // One past the cap opens exactly one more connection — demand over the
    // cap, never one socket per run and never the admission cap of 8.
    const five = await Promise.all(Array.from({ length: 5 }, () =>
      prefix.call({ export: 'slow', args: [150], limits: { cpuTimeMs: 5_000 } })))
    for (const r of five)
      expect(r.ok).toBe(true)
    expect((await sandbox.stats()).openConnections).toBe(2)
  })

  test('a run with pending waitUntil work frees its slot at the Result', async () => {
    // The #127 acceptance: background grace work must not hold admission
    // capacity. With ONE slot, run B can only execute while A's waitUntil
    // work is still parked if A's slot freed at its Result.
    await using sandbox = await createSandbox({ maxConcurrentRuns: 1 })

    let releaseAudit: () => void = () => {}
    const gate = new Promise<void>((resolve) => {
      releaseAudit = resolve
    })
    let auditDone = false

    const a = await sandbox.run({
      code: `
        waitUntil((async () => { await audit() })())
        export default 'a'
      `,
      globals: {
        audit: async () => {
          await gate
          auditDone = true
        },
      },
    })
    expect(a.ok).toBe(true)

    // A's grace work is still parked on the gated handler; B must run.
    const b = await sandbox.run({ code: 'export default "b"' })
    expect(b.ok).toBe(true)
    expect(auditDone).toBe(false)

    releaseAudit()
    if (a.ok) {
      const report = await a.waitUntil!
      expect(report.status).toBe('settled')
    }
    expect(auditDone).toBe(true)
  })

  test('passing the removed maxIsolates knob throws with the replacement named', async () => {
    await expect(

      createSandbox({ maxIsolates: 2 } as any),
    ).rejects.toThrow(/maxIsolates was removed.*maxConcurrentRuns/)
  })

  test('a nonsense maxConcurrentRuns is rejected instead of deadlocking', async () => {
    // 0 or a negative value would queue every run forever, and a fractional
    // value admits more runs than documented — all three throw before the
    // child is even spawned.
    for (const bad of [0, -1, 1.5]) {
      await expect(
        createSandbox({ maxConcurrentRuns: bad }),
      ).rejects.toThrow(/maxConcurrentRuns must be an integer/)
    }
  })
})

describe('stats()', () => {
  test('reports idle warm instances and their measured heap', async () => {
    await using sandbox = await createSandbox({ maxConcurrentRuns: 2 })
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
    await using single = await createSandbox({ maxConcurrentRuns: 1 })
    await using prefix = await single.prepare({ code: SLOW, globals: { hostSleep } })

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
    await using dual = await createSandbox({ maxConcurrentRuns: 2 })
    const busy = dual.run({
      globals: { hostSleep },
      code: 'await hostSleep(500)\nexport default 1',
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
