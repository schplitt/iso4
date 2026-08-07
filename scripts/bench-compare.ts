#!/usr/bin/env node
/* eslint-disable no-console */
/**
 * Compare a PR benchmark run against the stored main-branch baseline and
 * emit a markdown report for the sticky PR comment
 * (see .github/workflows/bench.yml).
 *
 * Interpretation caveat, also printed in the report: the baseline was
 * recorded by the last main-branch bench job on a DIFFERENT shared VM.
 * GitHub-hosted runners carry 10–30 % wall-time variance between jobs, so
 * deltas inside that band are noise — only slowdowns above `WARN_RATIO` get
 * flagged. For a trustworthy verdict on a specific change, run both sides
 * locally on the same machine (this script accepts any two result dirs).
 *
 * Inputs: two directories (base = stored baseline, head = PR run), each
 * containing any of
 *   node.json — Suite A, transformed by scripts/bench-transform.ts
 *               (bigger value = better)
 *   rust.txt  — Suite B, criterion `--output-format bencher` text
 *               (ns/iter: smaller value = better)
 *   meta.json — optional `{ sha, date }` stamp of the baseline commit
 *
 * A missing/empty base directory renders the PR numbers without ratios
 * (bootstrap case: no main baseline recorded yet).
 *
 * Usage: node scripts/bench-compare.ts <baseline-dir> <pr-dir> > comment.md
 */

import { existsSync, readFileSync } from 'node:fs'
import { join } from 'node:path'
import process from 'node:process'

/**
 * Slowdown ratio at/above which a row gets a ⚠️ marker. Matches the
 * cross-VM noise band of shared runners — below this it's not signal.
 */
const WARN_RATIO = 1.3

interface Metric {
  name: string
  value: number
  unit: string
  biggerIsBetter: boolean
}

const [, , baseDir, headDir] = process.argv
if (!baseDir || !headDir) {
  console.error('usage: bench-compare.ts <baseline-dir> <pr-dir>')
  process.exit(1)
}

function readNodeSuite(dir: string): Metric[] {
  const path = join(dir, 'node.json')
  if (!existsSync(path))
    return []
  const entries = JSON.parse(readFileSync(path, 'utf8')) as {
    name: string
    value: number
    unit: string
  }[]
  return entries.map((e) => ({ ...e, biggerIsBetter: true }))
}

function readRustSuite(dir: string): Metric[] {
  const path = join(dir, 'rust.txt')
  if (!existsSync(path))
    return []
  // Criterion bencher format: "test codec/encode/sparse1k ... bench: 123 ns/iter (+/- 45)"
  const metrics: Metric[] = []
  for (const line of readFileSync(path, 'utf8').split('\n')) {
    const m = /^test\s+(\S+)\s+\.\.\.\s+bench:\s+([\d,.]+)\s+(\S+)/.exec(line.trim())
    if (m) {
      metrics.push({
        name: m[1] as string,
        value: Number((m[2] as string).replaceAll(',', '')),
        unit: m[3] as string,
        biggerIsBetter: false,
      })
    }
  }
  return metrics
}

function readMeta(dir: string): { sha?: string, date?: string } {
  const path = join(dir, 'meta.json')
  if (!existsSync(path))
    return {}
  try {
    return JSON.parse(readFileSync(path, 'utf8')) as { sha?: string, date?: string }
  } catch {
    return {}
  }
}

function fmt(n: number): string {
  if (n >= 1000)
    return n.toLocaleString('en-US', { maximumFractionDigits: 0 })
  return n.toLocaleString('en-US', { maximumSignificantDigits: 4 })
}

interface Row {
  name: string
  base: string | undefined
  head: string
  /**
   * >1 = head slower than base, direction-normalised.
   * Undefined when the baseline has no matching entry.
   */
  slowdown: number | undefined
  unit: string
}

function compareSuite(base: Metric[], head: Metric[]): { rows: Row[], removed: string[] } {
  const baseByName = new Map(base.map((m) => [m.name, m]))
  const rows: Row[] = []
  for (const h of head) {
    const b = baseByName.get(h.name)
    const comparable = b !== undefined && b.value !== 0 && h.value !== 0
    rows.push({
      name: h.name,
      base: comparable ? fmt(b.value) : undefined,
      // Normalise so slowdown > 1 always means "PR is slower".
      slowdown: comparable
        ? (h.biggerIsBetter ? b.value / h.value : h.value / b.value)
        : undefined,
      head: fmt(h.value),
      unit: h.unit,
    })
  }
  const removed = base
    .filter((b) => !head.some((h) => h.name === b.name))
    .map((b) => b.name)
  return { rows, removed }
}

function renderTable(title: string, rows: Row[], betterNote: string): string {
  if (rows.length === 0)
    return ''
  const lines = [
    `### ${title}`,
    '',
    `<sup>${betterNote}</sup>`,
    '',
    '| benchmark | main | PR | PR vs main |',
    '| --- | ---: | ---: | ---: |',
  ]
  for (const r of rows) {
    let change = '—'
    if (r.slowdown !== undefined) {
      const pct = (r.slowdown - 1) * 100
      if (Math.abs(pct) < 0.05)
        change = '±0.0%'
      else if (pct > 0)
        change = `${pct.toFixed(1)}% slower${r.slowdown >= WARN_RATIO ? ' ⚠️' : ''}`
      else
        change = `${(-pct).toFixed(1)}% faster`
    }
    lines.push(
      `| \`${r.name}\` | ${r.base ? `${r.base} ${r.unit}` : '—'} | ${r.head} ${r.unit} | ${change} |`,
    )
  }
  lines.push('')
  return lines.join('\n')
}

const headNode = readNodeSuite(headDir)
const headRust = readRustSuite(headDir)

// No head metrics at all means the PR bench run itself broke — fail loudly.
if (headNode.length + headRust.length === 0) {
  console.error('[bench-compare] PR head produced no benchmark results')
  process.exit(1)
}

const baseNode = readNodeSuite(baseDir)
const baseRust = readRustSuite(baseDir)
const meta = readMeta(baseDir)

const node = compareSuite(baseNode, headNode)
const rust = compareSuite(baseRust, headRust)

const baselineLabel = baseNode.length + baseRust.length === 0
  ? '**No main baseline available yet** — showing PR numbers only. The next push to `main` records one.'
  : `Baseline: \`main@${(meta.sha ?? 'unknown').slice(0, 7)}\`${meta.date ? ` (${meta.date})` : ''}. `
    + 'Baseline and PR ran on different shared VMs (10–30 % wall-time variance) — '
    + `deltas inside that band are noise; ⚠️ marks slowdowns ≥ ${Math.round((WARN_RATIO - 1) * 100)} %. `
    + 'For a trustworthy verdict on a specific change, A/B locally with `scripts/bench-compare.ts`.'

const out: string[] = [
  '<!-- iso4-bench-ab -->',
  '## Benchmarks',
  '',
  baselineLabel,
  '',
  renderTable('Node end-to-end (Suite A)', node.rows, 'throughput — bigger is better'),
  renderTable('Rust micro (Suite B)', rust.rows, 'latency — smaller is better'),
]

const removed = [...node.removed, ...rust.removed]
if (removed.length > 0) {
  out.push(
    '<details><summary>Baseline benchmarks with no PR counterpart (renamed/removed?)</summary>',
    '',
    ...removed.map((n) => `- \`${n}\``),
    '',
    '</details>',
    '',
  )
}

console.log(out.filter(Boolean).join('\n'))
