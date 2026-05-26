/**
 * \@iso4/dynamic — public entry point.
 *
 * Spawns the Rust V8 subprocess, manages a pool of UDS connections (one per
 * isolate slot), and exposes the `Runtime` + `DynamicPrefix` API.
 */

import { access, unlink } from 'node:fs/promises'
import { cpus, tmpdir } from 'node:os'
import { join } from 'node:path'
import { randomUUID } from 'node:crypto'
import process from 'node:process'
import { spawn } from 'node:child_process'
import type { ChildProcess } from 'node:child_process'

import { resolveRuntimeBinary } from './binary'
import { RuntimeIpcClient } from './client'
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

// ── Public API ─────────────────────────────────────────────────────────────

export async function createRuntime(options?: RuntimeOptions): Promise<Runtime> {
  const binaryPath = resolveRuntimeBinary(options)
  const maxIsolates = options?.maxIsolates ?? cpus().length

  // Generate a per-process unique socket path and a cryptographically
  // random auth token. Both are passed as CLI args to the Rust binary so
  // no other process can predict the path or authenticate.
  const socketPath = join(tmpdir(), `iso4-v8-${process.pid}-${randomUUID().slice(0, 8)}.sock`)
  const token = randomUUID()

  const proc = spawn(binaryPath, ['--socket', socketPath, '--token', token], {
    // stdin closed, stdout ignored, stderr forwarded so runtime diagnostics
    // (the [iso4-v8] eprintln! lines) appear in the host process's stderr.
    stdio: ['ignore', 'ignore', 'inherit'],
  })

  proc.once('error', (err) => {
    process.stderr.write(`[iso4] failed to start V8 process: ${err.message}\n`)
  })

  await waitForSocket(socketPath)

  // Open all pool connections in parallel.
  const clients = await Promise.all(
    Array.from({ length: maxIsolates }, () =>
      RuntimeIpcClient.connect({ socketPath, token })),
  )

  const pool = new ConnectionPool(clients)
  return new RuntimeImpl(proc, pool, socketPath)
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
  private readonly socketPath: string
  private _alive = true

  constructor(proc: ChildProcess, pool: ConnectionPool, socketPath: string) {
    this.proc = proc
    this.pool = pool
    this.socketPath = socketPath
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
    // Best-effort cleanup: the Rust process leaves the socket file on disk
    // after it exits. Remove it; ignore the error if it's already gone.
    try {
      await unlink(this.socketPath)
    } catch {
      // already removed or never created — that's fine
    }
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
