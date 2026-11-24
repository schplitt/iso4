import { describe, expect, it } from 'vitest'
import { holm, mannWhitneyU, median, summarize } from '../bench/ab-stats.js'

const range = (from: number, n: number): number[] => Array.from({ length: n }, (_, i) => from + i)

describe('mannWhitneyU', () => {
  it('gives the exact p for clearly separated samples', () => {
    expect(mannWhitneyU([1, 2, 3], [4, 5, 6])).toEqual({ u: 0, p: 0.1 })
    // C(20, 10) = 184756 arrangements, one at each extreme.
    const r = mannWhitneyU(range(1, 10), range(11, 10))
    expect(r.u).toBe(0)
    expect(r.p).toBeCloseTo(2 / 184756, 12)
    expect(mannWhitneyU(range(11, 10), range(1, 10))).toEqual({ u: 100, p: r.p })
  })

  it('gives the exact p for interleaved samples without ties', () => {
    expect(mannWhitneyU([1, 2, 4], [3, 5, 6])).toEqual({ u: 1, p: 0.2 })
    expect(mannWhitneyU([1, 4, 5, 8], [2, 3, 6, 7]).p).toBe(1)
  })

  it('gives p = 1 for identical samples', () => {
    expect(mannWhitneyU([5, 5, 5], [5, 5, 5])).toEqual({ u: 4.5, p: 1 })
    expect(mannWhitneyU(range(1, 10), range(1, 10)).p).toBe(1)
  })

  it('uses the tie-corrected normal approximation with ties', () => {
    // U = 4.5, var = 25/12 * (11 - 18/90) = 22.5, z = 7.5 / sqrt(22.5).
    const r = mannWhitneyU([1, 2, 3, 4, 5], [3, 4, 5, 6, 7])
    expect(r.u).toBe(4.5)
    expect(r.p).toBeCloseTo(0.11385, 4)
  })
})

describe('holm', () => {
  it('scales by rank, keeps monotonic order, and returns the input order', () => {
    const adjusted = holm([0.01, 0.04, 0.03, 0.005])
    expect(adjusted.map((p) => Number(p.toFixed(10)))).toEqual([0.03, 0.06, 0.06, 0.02])
  })

  it('caps at 1', () => {
    expect(holm([0.5, 0.6])).toEqual([1, 1])
  })
})

describe('summarize', () => {
  it('reports medians, head vs base percent, and corrected significance', () => {
    const rows = summarize([
      { name: 'slower', base: range(1, 10), head: range(11, 10) },
      { name: 'same', base: range(1, 10), head: range(1, 10) },
    ])
    expect(median([3, 1, 2])).toBe(2)
    expect(rows[0]).toMatchObject({ name: 'slower', baseMedian: 5.5, headMedian: 15.5, significant: true })
    expect(rows[0]!.diffPct).toBeCloseTo(181.818, 2)
    expect(rows[0]!.p).toBeCloseTo(4 / 184756, 12)
    expect(rows[1]).toMatchObject({ name: 'same', diffPct: 0, p: 1, significant: false })
  })
})
