/**
 * Runtime benchmarks for \@iso4/sandbox.
 *
 * Four measurements across two describe blocks:
 *
 * cold / direct  — full cycle: spawn binary → connect pool → run → dispose.
 * cold / prefix  — full cycle: spawn → connect → precompile → prefix.run → dispose.
 *
 * hot  / direct  — binary already running, pool connected. runtime.run() only.
 * hot  / prefix  — binary running, prefix compiled. prefix.run() only.
 *
 * Run with:
 * pnpm bench
 */

import { afterAll, bench, describe } from 'vitest'
import { createSandbox as createRuntime } from '../src/index.js'
import type { Prefix as DynamicPrefix, Sandbox as Runtime } from '../src/index.js'
import { HEAVY_OPTS } from './profile.js'

const SAMPLE_CODE = 'export default 42'

// ── Hot run setup — top-level await so each runtime is ready before any
//    bench iteration fires. vitest executes the module fully during
//    collection, then runs the bench iterations.
//
//    Two separate runtimes so the benches don't queue behind each other
//    on a shared pool slot when vitest runs them concurrently.

const hotDirectRt: Runtime = await createRuntime({ maxIsolates: 1 })
const hotPrefixRt: Runtime = await createRuntime({ maxIsolates: 1 })
const hotPrefix: DynamicPrefix = await hotPrefixRt.precompile({ code: '' })

afterAll(async () => {
  await hotDirectRt.dispose()
  await hotPrefixRt.dispose()
})

// ── Cold start — binary spawn is part of every iteration ─────────────────
//
// warmupIterations: 0 — each iteration spawns a real binary; warmup would
//                        just add extra spawn overhead before the timed region.
// iterations           — small sample (see profile.ts); each call takes ~50–200 ms.

describe('cold start', () => {
  bench(
    'direct  (createRuntime → run → dispose)',
    async () => {
      const rt = await createRuntime({ maxIsolates: 1 })
      await rt.run({ code: SAMPLE_CODE })
      await rt.dispose()
    },
    HEAVY_OPTS,
  )

  bench(
    'prefix  (createRuntime → precompile → prefix.run → dispose)',
    async () => {
      const rt = await createRuntime({ maxIsolates: 1 })
      const prefix = await rt.precompile({ code: '' })
      await prefix.run({ code: SAMPLE_CODE })
      await rt.dispose()
    },
    HEAVY_OPTS,
  )
})

// ── Hot run — binary running, pool connected, only the call is timed ──────

describe('hot run', () => {
  bench('direct  (runtime.run — no snapshot)', async () => {
    await hotDirectRt.run({ code: SAMPLE_CODE })
  }, HEAVY_OPTS)

  bench('prefix  (prefix.run — from snapshot)', async () => {
    await hotPrefix.run({ code: SAMPLE_CODE })
  }, HEAVY_OPTS)
})
