#!/usr/bin/env node
/* eslint-disable no-console */
// Unpacks the pinned v8 crate into native/v8-runtime/vendor/v8 and applies
// patches/v8-liveness.patch; Cargo.toml's [patch.crates-io] points there.
import { Buffer } from 'node:buffer'
import { spawnSync } from 'node:child_process'
import { createHash } from 'node:crypto'
import { existsSync, mkdirSync, readFileSync, renameSync, rmSync, writeFileSync } from 'node:fs'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import process from 'node:process'

const VERSION = '152.2.0'
const SHA256 = 'a10fe1a92da5c32c7c7f838ce36c0ccfcfd5edf0865b58bdde820aa64cea9888'

const runtimeDir = resolve(dirname(fileURLToPath(import.meta.url)), '..', 'native', 'v8-runtime')
const vendorDir = join(runtimeDir, 'vendor')
const crateDir = join(vendorDir, 'v8')
const stampPath = join(vendorDir, 'v8.stamp')
const patchPath = join(runtimeDir, 'patches', 'v8-liveness.patch')

const stamp = `${VERSION} ${createHash('sha256').update(readFileSync(patchPath)).digest('hex')}`
if (existsSync(crateDir) && existsSync(stampPath) && readFileSync(stampPath, 'utf8') === stamp)
  process.exit(0)

const url = `https://static.crates.io/crates/v8/v8-${VERSION}.crate`
console.log(`[iso4] fetching ${url}`)
const response = await fetch(url)
if (!response.ok)
  throw new Error(`fetching ${url} failed: ${response.status}`)
const crate = Buffer.from(await response.arrayBuffer())
const actual = createHash('sha256').update(crate).digest('hex')
if (actual !== SHA256)
  throw new Error(`v8-${VERSION}.crate checksum mismatch: expected ${SHA256}, got ${actual}`)

rmSync(vendorDir, { recursive: true, force: true })
mkdirSync(vendorDir, { recursive: true })
const archivePath = join(vendorDir, `v8-${VERSION}.crate`)
writeFileSync(archivePath, crate)

for (const [cmd, args, cwd] of [
  ['tar', ['-xzf', archivePath], vendorDir],
  ['patch', ['-p1', '--forward', '--batch', '-i', patchPath], join(vendorDir, `v8-${VERSION}`)],
] as const) {
  const result = spawnSync(cmd, args, { cwd, stdio: 'inherit' })
  if (result.status !== 0)
    throw new Error(`${cmd} failed with status ${result.status}`)
}

rmSync(archivePath)
renameSync(join(vendorDir, `v8-${VERSION}`), crateDir)
writeFileSync(stampPath, stamp)
console.log(`[iso4] patched v8 ${VERSION} -> ${crateDir}`)
