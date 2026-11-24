// Counted tier: bytes and counts from Linux /proc for one child, written to counted.json.
// Usage: node bench/counted/counted.ts <iso4-v8 path> (or ISO4_V8_BIN); `pnpm bench:counted` wraps it in Docker.

import { execFileSync } from 'node:child_process'
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { dirname, join, resolve } from 'node:path'
import process from 'node:process'
import { setTimeout as sleep } from 'node:timers/promises'
import { fileURLToPath } from 'node:url'
import type * as SandboxModule from '../../src/index.js'

interface CountedRow { name: string, value: number, unit: 'bytes' | 'count' }
interface CountedRun { sha: string, date: string, rows: CountedRow[] }

interface ProcReading { privateBytes: number, pssBytes: number, threads: number }

const KB = 1024
const IDLE_WAIT_MS = 3000

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

  const sandbox = await createSandbox({ binaryPath: join(here, 'child.sh') })
  let idle: ProcReading
  try {
    await sleep(IDLE_WAIT_MS)
    idle = readProc(Number(readFileSync(pidFile, 'utf8')))
  } finally {
    await sandbox.dispose()
    rmSync(tmpDir, { recursive: true, force: true })
  }

  const run: CountedRun = {
    sha,
    date: new Date().toISOString(),
    rows: [
      { name: 'child.idle.private_bytes', value: idle.privateBytes, unit: 'bytes' },
      { name: 'child.idle.pss_bytes', value: idle.pssBytes, unit: 'bytes' },
      { name: 'child.idle.threads', value: idle.threads, unit: 'count' },
    ],
  }
  writeFileSync(outFile, `${JSON.stringify(run, null, 2)}\n`)
  for (const row of run.rows)
    process.stdout.write(`${row.name} ${row.value} ${row.unit}\n`)
}

if (import.meta.main)
  await main()
