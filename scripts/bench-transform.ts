#!/usr/bin/env node
/* eslint-disable no-console */
/**
 * Transform `vitest bench --outputJson` results into the flat metric list
 * stored as the main-branch baseline and consumed by
 * `scripts/bench-compare.ts` (bigger value = better):
 *
 *   [{ "name": "...", "unit": "ops/sec", "value": 1234.5, "range": "±1.2%", "extra": "..." }]
 *
 * Loop-mode convention: a bench whose name carries an `[xN]` suffix performs
 * N events per timed iteration (see bench/matrix.bench.ts). Its measured
 * ops/sec is multiplied by N and reported as events/sec so the chart tracks
 * per-event throughput.
 *
 * Usage: node scripts/bench-transform.ts <vitest-bench.json> <out.json>
 */

import { readFileSync, writeFileSync } from 'node:fs'
import process from 'node:process'

interface VitestBenchmark {
  name: string
  hz: number
  mean: number
  rme: number
  sampleCount?: number
  samples?: unknown[]
}

interface VitestGroup {
  fullName: string
  benchmarks: VitestBenchmark[]
}

interface VitestBenchFile {
  filepath?: string
  groups: VitestGroup[]
}

interface OutputEntry {
  name: string
  unit: string
  value: number
  range?: string
  extra?: string
}

const [, , inputPath, outputPath] = process.argv
if (!inputPath || !outputPath) {
  console.error('usage: bench-transform.ts <vitest-bench.json> <out.json>')
  process.exit(1)
}

const raw = JSON.parse(readFileSync(inputPath, 'utf8')) as {
  files?: VitestBenchFile[]
  testResults?: unknown
}

if (!Array.isArray(raw.files)) {
  console.error(
    `[bench-transform] no "files" array in ${inputPath} — was this produced by \`vitest bench --outputJson\`?`,
  )
  process.exit(1)
}

const entries: OutputEntry[] = []

for (const file of raw.files) {
  for (const group of file.groups ?? []) {
    // fullName is "<filepath> > <describe title>" (filepath may be relative
    // while file.filepath is absolute); the path segment is noise on charts,
    // the describe title is the suite name we want.
    const segments = group.fullName.split(' > ')
    const suite = segments[0]?.endsWith('.ts') || segments[0]?.endsWith('.js')
      ? segments.slice(1).join(' > ')
      : group.fullName
    for (const b of group.benchmarks ?? []) {
      const multiplier = /\[x(\d+)\]/.exec(b.name)
      const events = multiplier ? Number(multiplier[1]) : 1
      const samples = b.sampleCount ?? b.samples?.length
      entries.push({
        name: `${suite} > ${b.name}`,
        unit: events > 1 ? 'events/sec' : 'ops/sec',
        value: b.hz * events,
        range: `±${b.rme.toFixed(2)}%`,
        extra: `${samples ?? '?'} samples, mean ${b.mean.toFixed(4)} ms/iter`,
      })
    }
  }
}

if (entries.length === 0) {
  console.error(`[bench-transform] ${inputPath} contained no benchmark results`)
  process.exit(1)
}

writeFileSync(outputPath, `${JSON.stringify(entries, null, 2)}\n`)
console.log(`[bench-transform] wrote ${entries.length} entries to ${outputPath}`)
