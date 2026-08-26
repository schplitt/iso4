/**
 * End-to-end latency benchmarks across the payload matrix, plus loop-mode
 * throughput. See `bench/payloads.ts` for why these four shapes.
 *
 * per-run round trip — one full `runtime.run()` per iteration. The payload
 * enters the sandbox as a data global (host `serializeValue` → socket →
 * Rust `blob::deserialize_value`) and returns as the default export (Rust
 * `blob::serialize_value` → socket → host `deserializeValue`), so each
 * iteration measures both codec directions plus the fixed per-run tax.
 *
 * loop-mode — one long-lived run performs N bridge round trips: the sandbox
 * calls `next(prevResult)` in a loop and the host hands back the event.
 * Each round trip carries a payload in both directions. Bench names carry
 * an `[xN]` suffix; `scripts/bench-transform.ts` multiplies the measured
 * ops/sec by N to report events/sec.
 *
 * Every benched operation is probed once at module load and asserted `ok`,
 * and every timed body checks its own result too — `RunResult` failures
 * don't throw, and a run that starts failing mid-loop resolves fast, so an
 * unchecked bench would measure the error path and report it as a speedup.
 *
 * Run with the RELEASE native binary (`pnpm build:native`) — a debug
 * iso4-v8 invalidates every number here.
 */

import { afterAll, bench, describe } from 'vitest'
import { createSandbox } from '../src/index.js'
import type { RunResult, Sandbox } from '../src/index.js'
import { PAYLOAD_MATRIX, sparse1k } from './payloads.js'
import { HEAVY_OPTS, PR_PROFILE } from './profile.js'

const rt: Sandbox = await createSandbox({ maxIsolates: 1 })

afterAll(async () => {
  await rt.dispose()
})

function assertOk(result: RunResult, what: string): void {
  if (!result.ok) {
    throw new Error(
      `[bench sanity] ${what} did not complete ok: ${JSON.stringify(result.error)}`,
    )
  }
}

// ── Per-run latency × payload matrix ───────────────────────────────────────

const payloads = PAYLOAD_MATRIX.map(({ name, make }) => ({ name, payload: make() }))

for (const { name, payload } of payloads) {
  assertOk(
    await rt.run({
      code: 'export default EVENT',
      globals: { EVENT: { kind: 'data', value: payload } },
    }),
    `run round trip ${name}`,
  )
}

describe('run round trip', () => {
  for (const { name, payload } of payloads) {
    bench(
      name,
      async () => {
        assertOk(await rt.run({
          code: 'export default EVENT',
          globals: { EVENT: { kind: 'data', value: payload } },
        }), `bench ${name}`)
      },
      HEAVY_OPTS,
    )
  }
})

// ── Loop mode — bridge round trip per event ────────────────────────────────

const LOOP_EVENTS = PR_PROFILE ? 128 : 256

// `next(prev)` is one bridge round trip: `prev` travels sandbox → host,
// the returned event travels host → sandbox. The trivial transform keeps
// the measurement on the bridge, not on user code.
const LOOP_CODE = `
let result = null
for (let i = 0; i < ${LOOP_EVENTS}; i++) {
  const event = await next(result)
  result = { eventId: event.eventId, seq: i }
}
export default result
`

const loopEvent = sparse1k()

// Verify the loop really performs one bridge round trip per event before
// trusting any throughput number derived from it.
{
  const probe = await rt.run({
    code: LOOP_CODE,
    globals: { next: async () => loopEvent },
    limits: { maxBridgeCalls: LOOP_EVENTS },
  })
  assertOk(probe, 'loop mode')
  if (probe.ok) {
    if (probe.bridgeCalls.length !== LOOP_EVENTS) {
      throw new Error(
        `[bench sanity] loop mode made ${probe.bridgeCalls.length} bridge calls, expected ${LOOP_EVENTS}`,
      )
    }
    const last = probe.exports.default as { eventId?: string, seq?: number }
    if (last.eventId !== loopEvent.eventId || last.seq !== LOOP_EVENTS - 1) {
      throw new Error(`[bench sanity] loop mode export mismatch: ${JSON.stringify(last)}`)
    }
  }
}

describe('loop mode', () => {
  bench(
    `bridge round trip, sparse1k [x${LOOP_EVENTS}]`,
    async () => {
      assertOk(await rt.run({
        code: LOOP_CODE,
        globals: { next: async () => loopEvent },
        limits: { maxBridgeCalls: LOOP_EVENTS },
      }), 'bench loop mode')
    },
    HEAVY_OPTS,
  )
})
