/**
 * \@iso4/dynamic — public entry point.
 *
 * Spawns the Rust V8 subprocess, manages a pool of UDS connections (one per
 * isolate slot), and exposes the `Runtime` + `DynamicPrefix` API.
 */

import { access } from 'node:fs/promises'
import { cpus } from 'node:os'
import { spawn } from 'node:child_process'
import type { ChildProcess } from 'node:child_process'

import { resolveRuntimeBinary } from './binary'
import { RuntimeIpcClient, DEFAULT_SOCKET_PATH } from './client'
import type { PrecompilePayloadOptions, PrefixRunPayloadOptions } from './ipc'
import { ConnectionPool } from './pool'
import {
  decodePrecompileResultPayload,
  decodeRunCompletionPayload,
} from './wire'
import type {
  DynamicPrefix,
  PrecompileOptions,
  PrefixRunOptions,
  ResourceLimits,
  RunOptions,
  RunResult,
  Runtime,
  RuntimeOptions,
} from './types'

import process from 'node:process'

export type {
  // Re-exported from @iso4/core
  ResourceLimits,
  FetchHandler,
  HostFetchRequest,
  HostFetchResponse,
  HostGlobals,
  ImportsConfig,
  ImportDefinition,
  SourceImport,
  HostImport,
  HostExports,
  HostExportValue,
  HostExportData,
  HostExportFunction,

  // @iso4/dynamic-specific
  CreateRuntime,
  Runtime,
  RuntimeOptions,
  PrecompileOptions,
  DynamicPrefix,
  PrefixRunOptions,
  RunOptions,
  RunResult,
  RunSuccess,
  RunFailure,
  SandboxExports,
  RunError,
  RunErrorCode,
} from './types'

// Hardcoded for Phase 1. Configurable via env-var pass-through in a later
// phase once the socket path + token are no longer baked into the binary.
const DEV_TOKEN = 'dev-token'

// ── Public API ─────────────────────────────────────────────────────────────

export async function createRuntime(options?: RuntimeOptions): Promise<Runtime> {
  const binaryPath = resolveRuntimeBinary(options)
  const maxIsolates = options?.maxIsolates ?? cpus().length

  const proc = spawn(binaryPath, [], {
    // stdin closed, stdout ignored, stderr forwarded so runtime diagnostics
    // (the [iso4-v8] eprintln! lines) appear in the host process's stderr.
    stdio: ['ignore', 'ignore', 'inherit'],
  })

  proc.once('error', (err) => {
    process.stderr.write(`[iso4] failed to start V8 process: ${err.message}\n`)
  })

  await waitForSocket(DEFAULT_SOCKET_PATH)

  // Open all pool connections in parallel.
  const clients = await Promise.all(
    Array.from({ length: maxIsolates }, () =>
      RuntimeIpcClient.connect({ socketPath: DEFAULT_SOCKET_PATH, token: DEV_TOKEN })),
  )

  const pool = new ConnectionPool(clients)
  return new RuntimeImpl(proc, pool)
}

// ── Helpers ────────────────────────────────────────────────────────────────

async function waitForSocket(
  socketPath: string,
  timeoutMs = 5_000,
): Promise<void> {
  const deadline = Date.now() + timeoutMs
  while (Date.now() < deadline) {
    try {
      await access(socketPath)
      return
    } catch {
      await new Promise((resolve) => {
        setTimeout(resolve, 50)
      })
    }
  }
  throw new Error(
    `[iso4] V8 process socket not available after ${timeoutMs}ms: ${socketPath}`,
  )
}

/**
 * Map user-facing ResourceLimits to the wire-level encoding fields.
 * @param limits
 */
function toWireLimits(limits: Partial<ResourceLimits> | undefined): {
  memoryMb: number
  cpuTimeMs: number
  wallTimeMs: number
  maxExportBytes: number
  maxStdoutBytes: number
  maxStderrBytes: number
  maxBridgePayloadBytes: number
} {
  return {
    memoryMb: limits?.memoryMb ?? 0,
    cpuTimeMs: limits?.cpuTimeMs ?? 0,
    wallTimeMs: limits?.wallTimeMs ?? 0,
    maxExportBytes: 0,
    maxStdoutBytes: 0,
    maxStderrBytes: 0,
    maxBridgePayloadBytes: 0,
  }
}

// ── RuntimeImpl ────────────────────────────────────────────────────────────

class RuntimeImpl implements Runtime {
  private readonly proc: ChildProcess
  private readonly pool: ConnectionPool
  private _alive = true

  constructor(proc: ChildProcess, pool: ConnectionPool) {
    this.proc = proc
    this.pool = pool
    proc.once('exit', () => {
      this._alive = false
    })
  }

  get alive(): boolean {
    return this._alive
  }

  async run(options: RunOptions): Promise<RunResult> {
    return this.pool.withClient(async (client) => {
      const raw = await client.runRawCode(options.code, {
        filename: options.filename,
        limits: toWireLimits(options.limits),
      })
      return decodeRunCompletionPayload(raw.result).result
    })
  }

  async precompile(options: PrecompileOptions): Promise<DynamicPrefix> {
    const encodeOptions: PrecompilePayloadOptions = {
      code: options.code,
      filename: options.filename,
      limits: toWireLimits(options.limits),
    }

    return this.pool.withClient(async (client) => {
      const raw = await client.precompile(encodeOptions)
      const result = decodePrecompileResultPayload(raw)

      if (!result.ok) {
        const err = new Error(result.error.message) as Error & {
          code: string
          name: string
          stack?: string
        }
        err.name = result.error.name
        err.code = result.error.code
        if (result.error.stack)
          err.stack = result.error.stack
        throw err
      }

      return new DynamicPrefixImpl(result.prefixId, this.pool)
    })
  }

  async dispose(): Promise<void> {
    if (!this._alive)
      return
    this._alive = false
    await this.pool.dispose()
    this.proc.kill()
  }

  async [Symbol.asyncDispose](): Promise<void> {
    await this.dispose()
  }
}

// ── DynamicPrefixImpl ──────────────────────────────────────────────────────

class DynamicPrefixImpl implements DynamicPrefix {
  readonly id: string
  private readonly pool: ConnectionPool
  private _alive = true

  constructor(id: string, pool: ConnectionPool) {
    this.id = id
    this.pool = pool
  }

  get alive(): boolean {
    return this._alive
  }

  async run(options: PrefixRunOptions): Promise<RunResult> {
    if (!this._alive) {
      return {
        ok: false,
        error: {
          code: 'ERR_PREFIX_DISPOSED',
          name: 'Error',
          message: `prefix '${this.id}' has been disposed`,
        },
        stdout: [],
        stderr: [],
        durationMs: 0,
      }
    }

    const encodeOptions: PrefixRunPayloadOptions = {
      prefixId: this.id,
      code: options.code,
      filename: options.filename,
      limits: toWireLimits(options.limits),
    }

    return this.pool.withClient(async (client) => {
      const raw = await client.prefixRun(encodeOptions)
      return decodeRunCompletionPayload(raw.result).result
    })
  }

  async dispose(): Promise<void> {
    if (!this._alive)
      return
    this._alive = false
    // Fire-and-forget — no response frame for DisposePrefix.
    await this.pool.withClient((client) => client.disposePrefix(this.id))
  }

  async [Symbol.asyncDispose](): Promise<void> {
    await this.dispose()
  }
}
