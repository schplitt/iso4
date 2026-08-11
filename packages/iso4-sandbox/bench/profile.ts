/**
 * Bench iteration profiles.
 *
 * `ISO4_BENCH_PROFILE=pr` trims iteration counts for quick local runs
 * (e.g. a fast same-machine A/B while iterating on an optimization). For
 * numbers meant to be compared via scripts/bench-compare.ts, run the full
 * profile on both sides so they are collected the same way.
 */

import process from 'node:process'

export const PR_PROFILE: boolean = process.env.ISO4_BENCH_PROFILE === 'pr'

export interface BenchOpts {
  warmupIterations: number
  iterations: number
  time?: number
}

/**
 * For benches where one iteration is expensive (spawn, large payloads).
 */
export const HEAVY_OPTS: BenchOpts = PR_PROFILE
  ? { warmupIterations: 0, iterations: 15 }
  : { warmupIterations: 0, iterations: 50 }

/**
 * For sub-millisecond benches — let the time budget collect samples.
 */
export const MICRO_OPTS: BenchOpts = PR_PROFILE
  ? { warmupIterations: 2, iterations: 5, time: 250 }
  : { warmupIterations: 5, iterations: 10, time: 500 }
