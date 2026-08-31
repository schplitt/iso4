/**
 * Execution-model stress smoke — the permanent CI regression guard
 * distilled from the loop-under-load crash repro.
 *
 * The original crash (concurrent snapshot creation racing V8's process-shared
 * read-only heap) was resolved by REMOVING runtime snapshots, not by
 * locking. These tests pin the invariants the warm execution model must
 * hold under concurrent load, so a future change that reintroduces shared
 * mutable V8 state on a concurrent path fails here first — in every PR's
 * CI, without CPU saturation.
 *
 * The full acceptance bar (10/10 green with every core saturated) does not
 * fit shared CI runners without flaking; it lives in
 * `scripts/stress-local.sh` and is run on a real machine before merging
 * execution-model changes.
 *
 * Everything here is bounded: no unbounded spins, wall time well under the
 * suite timeout even on a loaded runner.
 */

import { describe, expect, test } from 'vitest'
import { createSandbox } from '../src/index.js'

const COUNTER = 'let n = 0\nexport function bump() { return ++n }'

describe('stress smoke: execution model under concurrent load', () => {
  test('concurrent prepare() calls all succeed (the historic crash vector)', async () => {
    // Concurrent precompiles on separate pool connections were exactly what
    // segfaulted the child under V8 14.7. prepare() no longer creates
    // snapshots; this pins that it stays safe to call concurrently.
    await using sandbox = await createSandbox({ maxIsolates: 4 })
    for (let round = 0; round < 3; round++) {
      const prefixes = await Promise.all(
        Array.from({ length: 8 }, (_, i) =>
          sandbox.prepare({ code: `export function tag() { return ${round * 8 + i} }` })),
      )
      const results = await Promise.all(
        prefixes.map(async (prefix, i) => {
          const result = await prefix.call({ export: 'tag' })
          await prefix.dispose()
          return result.ok && result.value === round * 8 + i
        }),
      )
      expect(results.every(Boolean)).toBe(true)
    }
  })

  test('taint/evict under concurrent load: blown calls fail alone, the rest succeed', async () => {
    // Interleave limit-blowing calls with healthy ones on one prefix across
    // two slots. Every blown call must fail with its limit code, every
    // healthy call must succeed, and afterwards the prefix must still serve
    // a clean call — eviction of tainted instances never poisons neighbors.
    await using sandbox = await createSandbox({ maxIsolates: 2 })
    await using prefix = await sandbox.prepare({
      code: `${COUNTER}\nexport function spin() { for (;;) {} }`,
    })

    const mixed = await Promise.all(
      Array.from({ length: 12 }, (_, i) =>
        i % 3 === 0
          ? prefix.call({
              export: 'spin',
              limits: { cpuTimeMs: 50, wallTimeMs: 5_000 },
            })
          : prefix.call({ export: 'bump' })),
    )

    for (const [i, result] of mixed.entries()) {
      if (i % 3 === 0) {
        expect(result.ok).toBe(false)
        if (result.ok)
          continue
        expect(['ERR_CPU_TIMEOUT', 'ERR_WALL_TIMEOUT']).toContain(
          result.error.code,
        )
      } else {
        expect(result.ok).toBe(true)
      }
    }

    const after = await prefix.call({ export: 'bump' })
    expect(after.ok).toBe(true)
  })

  test('eviction racing new admissions: hard pressure never breaks correctness', async () => {
    // memoryBudgetMb: 8 pins RSS over the hard mark, so the registry rejects
    // pooling while concurrent calls keep demanding instances — admission
    // and eviction race constantly. Calls must still all succeed, and state
    // must never leak between the cold instances (contract: correctness
    // never depends on warmth).
    await using pressured = await createSandbox({ maxIsolates: 2, memoryBudgetMb: 8 })
    const prefixes = await Promise.all(
      Array.from({ length: 3 }, () => pressured.prepare({ code: COUNTER })),
    )
    try {
      for (let round = 0; round < 3; round++) {
        const results = await Promise.all(
          prefixes.flatMap((prefix) => [
            prefix.call({ export: 'bump' }),
            prefix.call({ export: 'bump' }),
          ]),
        )
        for (const result of results) {
          expect(result.ok).toBe(true)
          // Cold isolate per call: the counter never advances past 1.
          if (result.ok)
            expect(result.value).toBe(1)
        }
      }
      const stats = await pressured.stats()
      expect(stats.underPressure).toBe(true)
      expect(stats.idleInstances).toBe(0)
    } finally {
      await Promise.all(prefixes.map((prefix) => prefix.dispose()))
    }
  })

  test('queue saturation: 32 concurrent calls on one slot all complete', async () => {
    // Saturation queues FIFO with no knobs; the guard is that a deep
    // queue drains completely — no starvation, no dropped calls — and that
    // stats() keeps answering from its control connection mid-saturation.
    await using single = await createSandbox({ maxIsolates: 1 })
    await using prefix = await single.prepare({ code: COUNTER })

    const flood = Promise.all(
      Array.from({ length: 32 }, () => prefix.call({ export: 'bump' })),
    )
    const stats = await single.stats()
    expect(stats.queueDepth).toBeGreaterThanOrEqual(0)

    const results = await flood
    expect(results.every((result) => result.ok)).toBe(true)
    // One slot, one warm instance: strictly serial execution, so the
    // counter must count every call exactly once.
    const values = results
      .map((result) => (result.ok ? result.value : -1))
      .sort((a, b) => a - b)
    expect(values).toEqual(Array.from({ length: 32 }, (_, i) => i + 1))
  })
})
