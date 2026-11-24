// Counted tier diff: prints a markdown table of main vs PR rows. A missing or bad base never fails; a bad head exits 1.
// Usage: node bench/counted/diff.ts <base counted.json, may be missing> <head counted.json>

import { existsSync, readFileSync } from 'node:fs'
import process from 'node:process'

import type { CountedRow, CountedRun } from './counted.js'

const MB = 1024 * 1024

function formatValue(row: CountedRow): string {
  if (row.unit === 'count')
    return String(Math.round(row.value))
  const mb = row.value / MB
  return `${mb.toFixed(mb < 100 ? 2 : 1)} MB`
}

function formatDiff(base: number, head: number): string {
  if (base === head)
    return '0'
  if (base === 0)
    return 'n/a'
  const pct = (head - base) / base * 100
  return `${pct > 0 ? '+' : ''}${pct.toFixed(1)}%`
}

const short = (sha: string): string => sha.slice(0, 7)

export function parseBase(text: string): CountedRun | 'unreadable' {
  try {
    const run = JSON.parse(text)
    return Array.isArray(run?.rows) ? run : 'unreadable'
  } catch {
    return 'unreadable'
  }
}

export function diffTable(base: CountedRun | 'unreadable' | undefined, head: CountedRun): string {
  if (!base || base === 'unreadable') {
    const lines = [
      `### Counted tier: PR \`${short(head.sha)}\``,
      '',
      base ? 'baseline unreadable, showing PR values only' : 'no baseline yet',
      '',
      '| row | PR |',
      '| --- | ---: |',
      ...head.rows.map((r) => `| ${r.name} | ${formatValue(r)} |`),
    ]
    return `${lines.join('\n')}\n`
  }
  const baseRows = new Map(base.rows.map((r) => [r.name, r]))
  const headNames = new Set(head.rows.map((r) => r.name))
  const lines = [
    `### Counted tier: main \`${short(base.sha)}\` vs PR \`${short(head.sha)}\``,
    '',
    '| row | main | PR | diff |',
    '| --- | ---: | ---: | ---: |',
  ]
  for (const r of head.rows) {
    const b = baseRows.get(r.name)
    lines.push(b
      ? `| ${r.name} | ${formatValue(b)} | ${formatValue(r)} | ${formatDiff(b.value, r.value)} |`
      : `| ${r.name} | - | ${formatValue(r)} | added |`)
  }
  for (const b of base.rows) {
    if (!headNames.has(b.name))
      lines.push(`| ${b.name} | ${formatValue(b)} | - | removed |`)
  }
  return `${lines.join('\n')}\n`
}

function main(): void {
  const [basePath, headPath] = process.argv.slice(2)
  if (!basePath || !headPath) {
    process.stderr.write('[counted] usage: node diff.ts <base counted.json> <head counted.json>\n')
    process.exit(1)
  }
  const base = existsSync(basePath) ? parseBase(readFileSync(basePath, 'utf8')) : undefined
  process.stdout.write(diffTable(base, JSON.parse(readFileSync(headPath, 'utf8'))))
}

if (import.meta.main)
  main()
