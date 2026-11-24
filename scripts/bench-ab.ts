#!/usr/bin/env node
/* eslint-disable no-console */
// Local A/B of the child benches: builds a release iso4-v8 from <base-ref> in a
// worktree under crates/target/bench-ab/base and one from this tree, runs 11
// interleaved base/head vitest processes in packages/iso4-sandbox (first pair
// dropped as warm-up), and prints a Mann-Whitney U table, Holm-corrected.
// Usage: pnpm bench:ab <base-ref> [vitest args...]   Never run in CI.
import { spawnSync } from 'node:child_process'
import type { SpawnSyncOptions } from 'node:child_process'
import { existsSync, mkdtempSync, readdirSync, readFileSync, realpathSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import process from 'node:process'
import { summarize } from '../packages/iso4-sandbox/bench/ab-stats.ts'
import type { RowSamples } from '../packages/iso4-sandbox/bench/ab-stats.ts'

const PAIRS = 11

const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), '..')
const sandboxDir = join(repoRoot, 'packages', 'iso4-sandbox')
const baseTree = join(repoRoot, 'crates', 'target', 'bench-ab', 'base')
const [ref, ...vitestArgs] = process.argv.slice(2)
const started = Date.now()

function fail(message: string): never {
  console.error(`[bench-ab] ${message}`)
  process.exit(1)
}

function progress(message: string): void {
  console.error(`[bench-ab] ${((Date.now() - started) / 1000).toFixed(0)}s ${message}`)
}

function run(cmd: string, args: string[], options: SpawnSyncOptions = {}): string {
  const r = spawnSync(cmd, args, { encoding: 'utf8', stdio: ['ignore', 'pipe', 'inherit'], ...options })
  if (r.status !== 0)
    fail(`${cmd} ${args.join(' ')} failed with status ${r.status ?? r.signal}`)
  return String(r.stdout ?? '').trim()
}

function git(args: string[]): string {
  return run('git', ['-C', repoRoot, ...args])
}

// prepare-v8 then the release build, cargo output on stderr; returns the binary path.
function build(tree: string): string {
  const targetDir = join(tree, 'crates', 'target')
  const opts: SpawnSyncOptions = { cwd: tree, stdio: ['ignore', process.stderr, 'inherit'] }
  run('node', ['scripts/prepare-v8.ts'], opts)
  run('cargo', ['build', '--release', '--manifest-path', 'crates/Cargo.toml', '--package', 'iso4-server', '--target-dir', targetDir], opts)
  return join(targetDir, 'release', 'iso4-v8')
}

if (!ref)
  fail('usage: bench-ab.ts <base-ref> [vitest args...]')

const resolved = spawnSync('git', ['-C', repoRoot, 'rev-parse', '--verify', '--quiet', '--end-of-options', `${ref}^{commit}`], { encoding: 'utf8' })
const baseSha = resolved.status === 0 ? resolved.stdout.trim() : ''
if (!baseSha)
  fail(`unknown ref: ${ref}`)
const headSha = git(['rev-parse', 'HEAD'])
const headLabel = `${headSha.slice(0, 12)}${git(['status', '--porcelain']) ? ' + uncommitted changes' : ''}`

if (existsSync(baseTree)) {
  // A folder that is not its own worktree would let git act on the main checkout.
  const top = spawnSync('git', ['-C', baseTree, 'rev-parse', '--show-toplevel'], { encoding: 'utf8' }).stdout?.trim()
  if (top !== realpathSync(baseTree))
    fail(`${baseTree} exists but is not a git worktree; remove that folder and run again`)
  run('git', ['-C', baseTree, 'checkout', '--quiet', '--force', '--detach', baseSha])
} else {
  git(['worktree', 'add', '--quiet', '--detach', baseTree, baseSha])
}

progress(`building base ${baseSha.slice(0, 12)} in ${baseTree}`)
const baseBin = build(baseTree)
progress('building head in this tree')
const headBin = build(repoRoot)
progress('builds done')

// The codec benches start no child; with no filter, select every other bench file.
const filters = vitestArgs.length > 0
  ? vitestArgs
  : readdirSync(join(sandboxDir, 'bench')).filter((f) => f.endsWith('.bench.ts') && f !== 'codec.bench.ts').map((f) => `bench/${f}`)

interface BenchJson {
  files: { groups: { fullName: string, benchmarks: { name: string, mean: number }[] }[] }[]
}

const tmp = mkdtempSync(join(tmpdir(), 'iso4-bench-ab-'))
const vitest = join(sandboxDir, 'node_modules', '.bin', 'vitest')
const samples = new Map<string, RowSamples>()
for (let pair = 0; pair < PAIRS; pair++) {
  for (const [side, bin] of [['base', baseBin], ['head', headBin]] as const) {
    progress(`pair ${pair + 1}/${PAIRS} ${side}${pair === 0 ? ' (warm-up)' : ''}`)
    const out = join(tmp, `${pair}-${side}.json`)
    const r = spawnSync(vitest, ['bench', '--run', '--outputJson', out, ...filters], {
      cwd: sandboxDir,
      encoding: 'utf8',
      maxBuffer: 64 * 1024 * 1024,
      env: { ...process.env, ISO4_BENCH_BIN: bin, ISO4_BENCH_PROFILE: 'pr' },
    })
    if (r.status !== 0) {
      const log = join(tmp, `${pair}-${side}.log`)
      writeFileSync(log, `${r.stdout ?? ''}${r.stderr ?? ''}`)
      fail(`bench run ${pair + 1} (${side}) failed with status ${r.status ?? r.signal}, output in ${log}`)
    }
    if (pair === 0)
      continue
    const json = JSON.parse(readFileSync(out, 'utf8')) as BenchJson
    for (const group of json.files.flatMap((f) => f.groups)) {
      for (const b of group.benchmarks) {
        const name = `${group.fullName} > ${b.name}`
        const row = samples.get(name) ?? { name, base: [], head: [] }
        row[side].push(b.mean)
        samples.set(name, row)
      }
    }
  }
}
progress('bench runs done')

const rows = [...samples.values()].filter((r) => r.base.length > 0 && r.head.length > 0)
for (const r of samples.values()) {
  if (!rows.includes(r))
    progress(`skipped ${r.name}: only on ${r.base.length > 0 ? 'base' : 'head'}`)
}
if (rows.length === 0)
  fail(`no bench row ran on both sides; outputs in ${tmp}`)
rmSync(tmp, { recursive: true, force: true })

const ms = (v: number): string => v.toPrecision(4)
const table = [['row', 'base ms', 'head ms', 'diff', 'p', ''], ...summarize(rows).map((r) => [
  r.name,
  ms(r.baseMedian),
  ms(r.headMedian),
  `${r.diffPct >= 0 ? '+' : ''}${r.diffPct.toFixed(1)}%`,
  r.p.toPrecision(2),
  r.significant ? '*' : '',
])]
const widths = table[0]!.map((_, c) => Math.max(...table.map((line) => line[c]!.length)))
console.log(`base ${baseSha.slice(0, 12)} vs head ${headLabel}, ${PAIRS - 1} pairs (1 warm-up dropped); medians of per-run means; diff = head vs base, negative = head faster; p Holm-corrected, * = p < 0.05`)
for (const line of table)
  console.log(line.map((cell, c) => c === 0 ? cell.padEnd(widths[c]!) : cell.padStart(widths[c]!)).join('  ').trimEnd())
