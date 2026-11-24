// Counted tier: bytes and counts from Linux /proc and stats(), one fresh child per scenario, written to counted.json.
// Usage: node bench/counted/counted.ts <iso4-v8 path> (or ISO4_V8_BIN); `pnpm bench:counted` wraps it in Docker.

import { execFileSync } from 'node:child_process'
import { existsSync, mkdtempSync, readFileSync, rmSync, statSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { dirname, join, resolve } from 'node:path'
import process from 'node:process'
import { setTimeout as sleep } from 'node:timers/promises'
import { fileURLToPath } from 'node:url'
import type * as SandboxModule from '../../src/index.js'

export interface CountedRow { name: string, value: number, unit: 'bytes' | 'count' }
export interface CountedRun { sha: string, date: string, rows: CountedRow[] }

interface ProcReading { privateBytes: number, pssBytes: number, threads: number }

const KB = 1024
const IDLE_WAIT_MS = 3000
const WARM_INSTANCES = 16

// smaps_rollup gives kB lines; private = Private_Clean + Private_Dirty.
export function parseProc(smapsRollup: string, status: string): ProcReading {
  const kb: Record<string, number> = {}
  for (const m of smapsRollup.matchAll(/^(\w+):\s+(\d+) kB$/gm))
    kb[m[1]!] = Number(m[2])
  const threads = /^Threads:\s+(\d+)$/m.exec(status)?.[1]
  if (kb.Pss === undefined || kb.Private_Clean === undefined || kb.Private_Dirty === undefined || threads === undefined)
    throw new Error('[counted] unexpected /proc format')
  return {
    privateBytes: (kb.Private_Clean + kb.Private_Dirty) * KB,
    pssBytes: kb.Pss * KB,
    threads: Number(threads),
  }
}

function readProc(pid: number): ProcReading {
  return parseProc(
    readFileSync(`/proc/${pid}/smaps_rollup`, 'utf8'),
    readFileSync(`/proc/${pid}/status`, 'utf8'),
  )
}

// An `mb`-sized array (the pool workload), kept at top level or dropped inside f(); k makes each prefix distinct.
function arrayCode(k: number, mb: number, keep: boolean): string {
  const make = `Array.from({ length: ${mb * 131072} }, (_, i) => i + ${k}.5)`
  return keep
    ? `const keep = ${make}\nexport function f() { return keep.length }`
    : `export function f() { return ${make}.length }`
}

// Per-instance PSS growth, and that growth minus the idle heap each instance reports.
export function warmRows(idle: ProcReading, warm: ProcReading, idleHeapBytes: number, instances: number): CountedRow[] {
  const pss = (warm.pssBytes - idle.pssBytes) / instances
  return [
    { name: 'warm.per_instance.pss_bytes', value: Math.round(pss), unit: 'bytes' },
    { name: 'warm.per_instance.overhead_bytes', value: Math.round(pss - idleHeapBytes / instances), unit: 'bytes' },
    { name: `warm.at${instances}.threads`, value: warm.threads, unit: 'count' },
  ]
}

async function main(): Promise<void> {
  if (!existsSync('/proc/self/smaps_rollup')) {
    process.stderr.write('[counted] needs Linux /proc; run `pnpm bench:counted` (Docker) instead\n')
    process.exit(1)
  }
  const bin = process.argv[2] ?? process.env.ISO4_V8_BIN
  if (!bin) {
    process.stderr.write('[counted] usage: node counted.ts <path to iso4-v8> (or set ISO4_V8_BIN)\n')
    process.exit(1)
  }

  const here = dirname(fileURLToPath(import.meta.url))
  const pkgDir = resolve(here, '../..')
  const outFile = join(pkgDir, 'counted.json')
  rmSync(outFile, { force: true })
  const sha = process.env.COUNTED_SHA ?? execFileSync('git', ['rev-parse', 'HEAD'], { encoding: 'utf8' }).trim()
  const distUrl = new URL('../../dist/index.mjs', import.meta.url).href
  const { createSandbox }: typeof SandboxModule = await import(distUrl)

  // child.sh reads these from the inherited env.
  const tmpDir = mkdtempSync(join(tmpdir(), 'iso4-counted-'))
  const pidFile = join(tmpDir, 'child.pid')
  process.env.ISO4_COUNTED_BIN = resolve(bin)
  process.env.ISO4_COUNTED_PID_FILE = pidFile

  // A fresh child per scenario; settleSecs goes to --idle-settle-secs.
  async function withChild<T>(settleSecs: number, options: SandboxModule.SandboxOptions, body: (sandbox: SandboxModule.Sandbox, pid: () => number) => Promise<T>): Promise<T> {
    process.env.ISO4_COUNTED_SETTLE_SECS = String(settleSecs)
    const sandbox = await createSandbox({ ...options, binaryPath: join(here, 'child.sh') })
    try {
      return await body(sandbox, () => Number(readFileSync(pidFile, 'utf8')))
    } finally {
      await sandbox.dispose()
    }
  }

  async function prepareAndCall(sandbox: SandboxModule.Sandbox, code: string, memoryMb: number): Promise<void> {
    const prefix = await sandbox.prepare({ code, memoryMb })
    const result = await prefix.call({ export: 'f' })
    if (!result.ok)
      throw new Error(`[counted] prefix call failed: ${result.error.code}`)
  }

  async function idleChild(): Promise<CountedRow[]> {
    return withChild(1, {}, async (_sandbox, pid) => {
      await sleep(IDLE_WAIT_MS)
      const idle = readProc(pid())
      return [
        { name: 'child.idle.private_bytes', value: idle.privateBytes, unit: 'bytes' },
        { name: 'child.idle.pss_bytes', value: idle.pssBytes, unit: 'bytes' },
        { name: 'child.idle.threads', value: idle.threads, unit: 'count' },
      ]
    })
  }

  async function warm(): Promise<CountedRow[]> {
    return withChild(1, { memoryBudgetMb: 0, hostReserveMb: 0 }, async (sandbox, pid) => {
      await sleep(IDLE_WAIT_MS)
      const idle = readProc(pid())
      for (let k = 0; k < WARM_INSTANCES; k++)
        await prepareAndCall(sandbox, arrayCode(k, 4, true), 24)
      await sleep(IDLE_WAIT_MS)
      const stats = await sandbox.stats()
      const at16 = readProc(pid())
      if (stats.warmInstances !== WARM_INSTANCES || stats.idleInstances !== WARM_INSTANCES)
        throw new Error(`[counted] expected ${WARM_INSTANCES} idle warm instances, got ${stats.idleInstances}/${stats.warmInstances}`)
      return warmRows(idle, at16, stats.idleHeapBytes, WARM_INSTANCES)
    })
  }

  // One call that drops a ~16 MB array; raw = settle off, settled = after the 1 s settle GC.
  async function garbage(settleSecs: number): Promise<number> {
    return withChild(settleSecs, {}, async (sandbox) => {
      await prepareAndCall(sandbox, arrayCode(0, 16, false), 64)
      await sleep(IDLE_WAIT_MS)
      const stats = await sandbox.stats()
      if (stats.idleInstances !== 1)
        throw new Error(`[counted] expected 1 idle instance for the garbage reading, got ${stats.idleInstances}`)
      return stats.idleHeapBytes
    })
  }

  const rows: CountedRow[] = []
  try {
    rows.push(...await idleChild())
    rows.push(...await warm())
    const raw = await garbage(0)
    const settled = await garbage(1)
    rows.push({ name: 'garbage.idle_heap_raw_bytes', value: raw, unit: 'bytes' })
    rows.push({ name: 'garbage.idle_heap_settled_bytes', value: settled, unit: 'bytes' })
    rows.push({ name: 'binary.bytes', value: statSync(resolve(bin)).size, unit: 'bytes' })
  } finally {
    rmSync(tmpDir, { recursive: true, force: true })
  }

  const run: CountedRun = { sha, date: new Date().toISOString(), rows }
  writeFileSync(outFile, `${JSON.stringify(run, null, 2)}\n`)
  for (const row of run.rows)
    process.stdout.write(`${row.name} ${row.value} ${row.unit}\n`)
}

if (import.meta.main)
  await main()
