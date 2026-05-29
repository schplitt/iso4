#!/usr/bin/env node
/* eslint-disable no-console */
import { spawnSync } from 'node:child_process'
import { chmodSync, copyFileSync, existsSync, mkdirSync } from 'node:fs'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import process from 'node:process'

const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), '..')
const args: string[] = process.argv.slice(2)
const release: boolean = args.includes('--release')
const targetArgIndex: number = args.indexOf('--target')
const target: string | null = targetArgIndex === -1 ? null : (args[targetArgIndex + 1] ?? null)

if (targetArgIndex !== -1 && !target) {
  throw new Error('--target requires a Rust target triple')
}

const platformPackage = currentPlatformPackage()
const manifestPath = join(
  repoRoot,
  'native',
  'v8-runtime',
  'Cargo.toml',
)

const cargoArgs = ['build', '--manifest-path', manifestPath]
if (release)
  cargoArgs.push('--release')
if (target)
  cargoArgs.push('--target', target)

console.log(`[iso4] cargo ${cargoArgs.join(' ')}`)
const cargo = spawnSync('cargo', cargoArgs, {
  cwd: repoRoot,
  stdio: 'inherit',
})

if (cargo.status !== 0) {
  process.exit(cargo.status ?? 1)
}

const profile = release ? 'release' : 'debug'
const targetDir = target
  ? join(repoRoot, 'native', 'v8-runtime', 'target', target, profile)
  : join(repoRoot, 'native', 'v8-runtime', 'target', profile)
const sourceBinary = join(targetDir, 'iso4-v8')

if (!existsSync(sourceBinary)) {
  throw new Error(`cargo build completed but binary was not found at ${sourceBinary}`)
}

const destinationDir = join(repoRoot, 'packages', platformPackage, 'bin')
const destinationBinary = join(destinationDir, 'iso4-v8')

mkdirSync(destinationDir, { recursive: true })
copyFileSync(sourceBinary, destinationBinary)
chmodSync(destinationBinary, 0o755)

console.log(`[iso4] copied ${sourceBinary}`)
console.log(`[iso4]     -> ${destinationBinary}`)

// On macOS, ad-hoc re-sign after copying. The copy invalidates the original
// code signature (macOS tracks the signature against the file content + path).
// Without this the binary is killed immediately on launch with
// SIGKILL / EXC_BAD_ACCESS "Code Signature Invalid".
if (process.platform === 'darwin') {
  const codesign = spawnSync('codesign', ['--sign', '-', '--force', destinationBinary], {
    cwd: repoRoot,
    stdio: 'inherit',
  })
  if (codesign.status !== 0) {
    throw new Error(`codesign failed with status ${codesign.status}`)
  }
  console.log(`[iso4] ad-hoc signed ${destinationBinary}`)
}

function currentPlatformPackage(): string {
  if (process.platform === 'darwin' && process.arch === 'arm64') {
    return 'iso4-v8-darwin-arm64'
  }
  if (process.platform === 'darwin' && process.arch === 'x64') {
    return 'iso4-v8-darwin-x64'
  }
  if (process.platform === 'linux' && process.arch === 'arm64') {
    return 'iso4-v8-linux-arm64-gnu'
  }
  if (process.platform === 'linux' && process.arch === 'x64') {
    return 'iso4-v8-linux-x64-gnu'
  }

  throw new Error(
    `Unsupported native build platform: ${process.platform}/${process.arch}. `
    + '@iso4/sandbox supports darwin/arm64, darwin/x64, linux/arm64, and linux/x64.',
  )
}
