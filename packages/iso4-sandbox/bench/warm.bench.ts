/**
 * Warm-path benchmarks: `prefix.call()` latency and same-prefix throughput.
 *
 * These are the numbers the warm-isolate registry (#64) is meant to move —
 * today every call pays isolate boot + prefix re-evaluation + teardown; a
 * warm instance skips all three. Run once before and once after on the same
 * idle machine for the A/B.
 *
 * call latency — one `prefix.call()` per iteration, maxIsolates: 1:
 *   sync handler / empty prefix    — floor: the pure per-call tax.
 *   async handler / empty prefix   — adds the settle/poll machinery.
 *   sync handler / realistic prefix — adds prefix re-evaluation, the part
 *                                     that scales with prefix size.
 *
 * throughput — one iteration fires CONCURRENCY×BATCH calls across an
 * 8-slot pool; the `[xN]` suffix makes `scripts/bench-transform.ts` report
 * events/sec. Two variants: all calls on one prefix (the warm sweet spot)
 * and interleaved across two prefixes (post-warm: catches eviction thrash).
 *
 * Run with the RELEASE native binary (`pnpm build:native`).
 */

import { afterAll, bench, describe } from 'vitest'
import { createSandbox } from '../src/index.js'
import type { CallResult, Prefix, Sandbox } from '../src/index.js'
import { HEAVY_OPTS } from './profile.js'

const SYNC_HANDLER = `
export default {
  fetch(event) {
    return { seen: event.n, tag: 'sync' }
  },
}
`

const ASYNC_HANDLER = `
export default {
  async fetch(event) {
    const doubled = await Promise.resolve(event.n * 2)
    return { seen: doubled, tag: 'async' }
  },
}
`

/**
 * A prefix shaped like real tenant setup code: constants, a couple of helper
 * functions, a lookup table built at module scope. ~2 KB of source whose
 * evaluation cost warm mode eliminates from the per-call path.
 */
const REALISTIC_PREFIX = `
const CONFIG = Object.freeze({
  tenant: 'bench-tenant',
  region: 'eu-central-1',
  flags: { retries: 3, verbose: false, batching: true },
})

const ROUTES = new Map()
for (let i = 0; i < 200; i++) {
  ROUTES.set('route-' + i, { weight: (i * 7919) % 101, bucket: i % 8 })
}

function score(route, n) {
  const r = ROUTES.get('route-' + (n % 200))
  return r ? r.weight * route.length + r.bucket : 0
}

function normalize(event) {
  return { n: event.n | 0, key: 'route-' + ((event.n | 0) % 200) }
}

export default {
  fetch(event) {
    const e = normalize(event)
    return { seen: e.n, score: score(e.key, e.n), tag: 'realistic' }
  },
}
`

function assertOk(result: CallResult, what: string): void {
  if (!result.ok) {
    throw new Error(
      `[bench sanity] ${what} did not complete ok: ${JSON.stringify(result.error)}`,
    )
  }
}

// ── Call latency — one call per iteration, single slot ─────────────────────

const latencyRt: Sandbox = await createSandbox({ maxIsolates: 1 })
const syncPrefix: Prefix = await latencyRt.prepare({ code: SYNC_HANDLER })
const asyncPrefix: Prefix = await latencyRt.prepare({ code: ASYNC_HANDLER })
const realisticPrefix: Prefix = await latencyRt.prepare({ code: REALISTIC_PREFIX })

assertOk(
  await syncPrefix.call({ export: 'default.fetch', args: [{ n: 1 }] }),
  'call sync handler',
)
assertOk(
  await asyncPrefix.call({ export: 'default.fetch', args: [{ n: 1 }] }),
  'call async handler',
)
assertOk(
  await realisticPrefix.call({ export: 'default.fetch', args: [{ n: 1 }] }),
  'call realistic prefix',
)

describe('call latency', () => {
  bench('sync handler, empty prefix', async () => {
    await syncPrefix.call({ export: 'default.fetch', args: [{ n: 7 }] })
  }, HEAVY_OPTS)

  bench('async handler, empty prefix', async () => {
    await asyncPrefix.call({ export: 'default.fetch', args: [{ n: 7 }] })
  }, HEAVY_OPTS)

  bench('sync handler, realistic prefix (~2KB)', async () => {
    await realisticPrefix.call({ export: 'default.fetch', args: [{ n: 7 }] })
  }, HEAVY_OPTS)
})

// ── Throughput — concurrent calls across an 8-slot pool ────────────────────

const SLOTS = 8
const BATCH = 64

const throughputRt: Sandbox = await createSandbox({ maxIsolates: SLOTS })
const tpPrefixA: Prefix = await throughputRt.prepare({ code: REALISTIC_PREFIX })
const tpPrefixB: Prefix = await throughputRt.prepare({ code: SYNC_HANDLER })

assertOk(
  await tpPrefixA.call({ export: 'default.fetch', args: [{ n: 1 }] }),
  'throughput prefix A',
)
assertOk(
  await tpPrefixB.call({ export: 'default.fetch', args: [{ n: 1 }] }),
  'throughput prefix B',
)

afterAll(async () => {
  await latencyRt.dispose()
  await throughputRt.dispose()
})

describe('call throughput', () => {
  bench(`same prefix, ${SLOTS} slots [x${BATCH}]`, async () => {
    await Promise.all(
      Array.from({ length: BATCH }, (_, i) =>
        tpPrefixA.call({ export: 'default.fetch', args: [{ n: i }] })),
    )
  }, HEAVY_OPTS)

  bench(`two prefixes interleaved, ${SLOTS} slots [x${BATCH}]`, async () => {
    await Promise.all(
      Array.from({ length: BATCH }, (_, i) =>
        (i % 2 === 0 ? tpPrefixA : tpPrefixB)
          .call({ export: 'default.fetch', args: [{ n: i }] })),
    )
  }, HEAVY_OPTS)
})
