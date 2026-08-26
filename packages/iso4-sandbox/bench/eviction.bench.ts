/**
 * Eviction-path benchmarks (#67): what a call costs when warmth is NOT there.
 *
 * Together with `warm.bench.ts` (the warm hit) and `runtime.bench.ts`
 * (one-off and full cold start), this completes the issue-#67 matrix:
 *
 *   warm hit            — warm.bench.ts `call latency`
 *   one-off run         — runtime.bench.ts `hot run / direct`
 *   full cold start     — runtime.bench.ts `cold start`
 *   cold instance       — HERE: boot + prefix eval, no process spawn
 *   pressure degradation— HERE: the same path, entered via the watermarks
 *
 * cold instance per call — `memoryBudgetMb: 8` pins RSS over the hard mark
 * from the first sample, so nothing is ever pooled and every `prefix.call()`
 * pays isolate boot + prefix evaluation (capacity.test.ts proves the
 * semantics; this file prices them). This is BOTH interesting numbers at
 * once: the admission cost a call pays right after its instance was evicted,
 * and the degraded-mode latency under sustained memory pressure. The delta
 * against the warm hit in warm.bench.ts is the value warmth adds.
 *
 * Deliberately NOT benched: the taint→evict cycle (a call that blows a CPU
 * limit). Its cost is dominated by the blown call's own limit budget, so the
 * number would measure the limit, not the eviction. The eviction machinery
 * itself (pick_victim, watermark_action) is CodSpeed-tracked at nanosecond
 * resolution in the Rust `policy/*` group.
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

/**
 * Same shape as warm.bench.ts so the warm/cold delta is like-for-like.
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

// ── Cold instance per call — hard pressure disables pooling ────────────────

const pressured: Sandbox = await createSandbox({ maxIsolates: 1, memoryBudgetMb: 8 })
const coldSync: Prefix = await pressured.prepare({ code: SYNC_HANDLER })
const coldRealistic: Prefix = await pressured.prepare({ code: REALISTIC_PREFIX })

assertOk(
  await coldSync.call({ export: 'default.fetch', args: [{ n: 1 }] }),
  'cold call sync handler',
)
assertOk(
  await coldRealistic.call({ export: 'default.fetch', args: [{ n: 1 }] }),
  'cold call realistic prefix',
)

// Sanity: the pressure gate must actually be closed, otherwise every number
// below silently measures the warm path and the bench is garbage.
{
  const stats = await pressured.stats()
  if (!stats.underPressure || stats.idleInstances !== 0) {
    throw new Error(
      `[bench sanity] expected hard pressure with no pooling, got ${JSON.stringify(stats)}`,
    )
  }
}

afterAll(async () => {
  await pressured.dispose()
})

describe('eviction path: cold instance per call', () => {
  bench('sync handler, empty prefix (boot + eval per call)', async () => {
    assertOk(await coldSync.call({ export: 'default.fetch', args: [{ n: 7 }] }), 'bench cold sync call')
  }, HEAVY_OPTS)

  bench('sync handler, realistic prefix ~2KB (boot + eval per call)', async () => {
    assertOk(await coldRealistic.call({ export: 'default.fetch', args: [{ n: 7 }] }), 'bench cold realistic call')
  }, HEAVY_OPTS)
})
