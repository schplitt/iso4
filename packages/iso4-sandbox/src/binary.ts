import { existsSync } from 'node:fs'
import process from 'node:process'
import { fileURLToPath } from 'node:url'

import type { SandboxOptions } from './types'

interface PlatformBinaryPackage {
  packageName: string
  binarySpecifier: string
}

export function resolveRuntimeBinary(options: SandboxOptions = {}): string {
  if (options.binaryPath) {
    return options.binaryPath
  }

  const platformPackage = platformBinaryPackage()
  let binaryPath: string

  try {
    binaryPath = fileURLToPath(import.meta.resolve(platformPackage.binarySpecifier))
  } catch {
    throw new Error(
      `Missing iso4-v8 binary package for ${process.platform}/${process.arch}: `
      + `${platformPackage.packageName}. Install it with:\n\n`
      + `  npm install ${platformPackage.packageName}\n\n`
      + 'or pass SandboxOptions.binaryPath.',
    )
  }

  if (!existsSync(binaryPath)) {
    throw new Error(
      `The iso4-v8 binary package ${platformPackage.packageName} is installed, `
      + `but its binary was not found at ${binaryPath}.`,
    )
  }

  return binaryPath
}

function platformBinaryPackage(): PlatformBinaryPackage {
  if (process.platform === 'darwin' && process.arch === 'arm64') {
    return {
      packageName: '@iso4/v8-darwin-arm64',
      binarySpecifier: '@iso4/v8-darwin-arm64/bin/iso4-v8',
    }
  }
  if (process.platform === 'darwin' && process.arch === 'x64') {
    return {
      packageName: '@iso4/v8-darwin-x64',
      binarySpecifier: '@iso4/v8-darwin-x64/bin/iso4-v8',
    }
  }
  if (process.platform === 'linux' && process.arch === 'arm64') {
    return {
      packageName: '@iso4/v8-linux-arm64-gnu',
      binarySpecifier: '@iso4/v8-linux-arm64-gnu/bin/iso4-v8',
    }
  }
  if (process.platform === 'linux' && process.arch === 'x64') {
    return {
      packageName: '@iso4/v8-linux-x64-gnu',
      binarySpecifier: '@iso4/v8-linux-x64-gnu/bin/iso4-v8',
    }
  }

  throw new Error(
    `Unsupported platform: ${process.platform}/${process.arch}. `
    + '@iso4/sandbox supports darwin/arm64, darwin/x64, linux/arm64, and linux/x64.',
  )
}
