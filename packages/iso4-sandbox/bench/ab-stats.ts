/**
 * Statistics for `scripts/bench-ab.ts`: Mann-Whitney U per row, Holm
 * correction across rows. Pure functions, tested in tests/ab-stats.test.ts.
 */

export interface UTest {
  u: number
  p: number
}

export interface RowSamples {
  name: string
  base: number[]
  head: number[]
}

export interface RowSummary {
  name: string
  baseMedian: number
  headMedian: number
  diffPct: number
  p: number
  significant: boolean
}

// Two-sided Mann-Whitney U. Exact p without ties, else the normal
// approximation with tie and continuity correction.
export function mannWhitneyU(a: number[], b: number[]): UTest {
  const n1 = a.length
  const n2 = b.length
  const all = [...a.map((v) => ({ v, inA: true })), ...b.map((v) => ({ v, inA: false }))]
    .sort((x, y) => x.v - y.v)
  let rankSumA = 0
  let tieTerm = 0
  for (let i = 0; i < all.length;) {
    let j = i
    while (j + 1 < all.length && all[j + 1]!.v === all[i]!.v)
      j++
    const rank = (i + j) / 2 + 1
    for (let k = i; k <= j; k++) {
      if (all[k]!.inA)
        rankSumA += rank
    }
    const t = j - i + 1
    tieTerm += t * t * t - t
    i = j + 1
  }
  const u = rankSumA - n1 * (n1 + 1) / 2
  const n = n1 + n2
  if (tieTerm === 0)
    return { u, p: exactP(u, n1, n2) }
  const variance = n1 * n2 / 12 * (n + 1 - tieTerm / (n * (n - 1)))
  const diff = Math.max(Math.abs(u - n1 * n2 / 2) - 0.5, 0)
  if (variance === 0 || diff === 0)
    return { u, p: 1 }
  return { u, p: Math.min(1, 2 * (1 - normalCdf(diff / Math.sqrt(variance)))) }
}

// Holm step-down adjustment; returns adjusted p in the input order.
export function holm(ps: number[]): number[] {
  const m = ps.length
  const order = ps.map((p, i) => ({ p, i })).sort((x, y) => x.p - y.p)
  const out = Array.from<number>({ length: m })
  let running = 0
  order.forEach(({ p, i }, k) => {
    running = Math.max(running, Math.min(1, (m - k) * p))
    out[i] = running
  })
  return out
}

export function summarize(rows: RowSamples[], alpha = 0.05): RowSummary[] {
  const adjusted = holm(rows.map((r) => mannWhitneyU(r.base, r.head).p))
  return rows.map((r, i) => {
    const baseMedian = median(r.base)
    const headMedian = median(r.head)
    const p = adjusted[i]!
    return {
      name: r.name,
      baseMedian,
      headMedian,
      diffPct: (headMedian / baseMedian - 1) * 100,
      p,
      significant: p < alpha,
    }
  })
}

export function median(values: number[]): number {
  const s = [...values].sort((x, y) => x - y)
  const mid = Math.floor(s.length / 2)
  return s.length % 2 === 1 ? s[mid]! : (s[mid - 1]! + s[mid]!) / 2
}

// Two-sided exact p from the count of rank arrangements per U value.
function exactP(u: number, n1: number, n2: number): number {
  // counts[i][j][k]: arrangements of i and j values with U = k.
  const counts: number[][][] = []
  for (let i = 0; i <= n1; i++) {
    counts.push([])
    for (let j = 0; j <= n2; j++) {
      const row = Array.from<number>({ length: i * j + 1 }).fill(0)
      if (i === 0 || j === 0) {
        row[0] = 1
      } else {
        for (let k = 0; k <= i * j; k++)
          row[k] = (counts[i - 1]![j]![k - j] ?? 0) + (counts[i]![j - 1]![k] ?? 0)
      }
      counts[i]!.push(row)
    }
  }
  const dist = counts[n1]![n2]!
  const total = dist.reduce((s, c) => s + c, 0)
  const lo = Math.min(u, n1 * n2 - u)
  let tail = 0
  for (let k = 0; k <= lo; k++)
    tail += dist[k]!
  return Math.min(1, 2 * tail / total)
}

// Abramowitz and Stegun 7.1.26, absolute error below 1.5e-7.
function normalCdf(z: number): number {
  const x = Math.abs(z) / Math.SQRT2
  const t = 1 / (1 + 0.3275911 * x)
  const poly = t * (0.254829592 + t * (-0.284496736 + t * (1.421413741 + t * (-1.453152027 + t * 1.061405429))))
  const erf = 1 - poly * Math.exp(-x * x)
  return z >= 0 ? (1 + erf) / 2 : (1 - erf) / 2
}
