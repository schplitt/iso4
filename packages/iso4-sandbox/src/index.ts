/**
 * \@iso4/sandbox - public entry point.
 *
 * Spawns the Rust V8 subprocess, manages a pool of UDS connections (one per
 * isolate slot), and exposes the `Sandbox` + `Prefix` API.
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

import { ConnectionPool } from './pool'
import {
  decodePrecompileResultPayload,
  decodeRunCompletionPayload,
} from './wire'
import type {
  HostGlobals,
  PrecompileOptions,
  PrefixRunOptions,
  RebindGlobals,
  Prefix,
  ResourceLimits,
  RunOptions,
  RunResult,
  Sandbox,
  SandboxOptions,
  Imports,
} from './types'

// ── Global processing (imported from globals.ts for testability) ──────────

import { extractBridgeGlobals, processGlobals } from './globals.js'
import {
  BRIDGE_DISPATCH_GLOBAL,
  UndeclaredImportBindingError,
  createDispatchGlobal,
  extractRebindImports,
  processImports,
} from './imports.js'
import type { DeclaredImportShape, HandleRegistry } from './imports.js'

export type {
  ResourceLimits,
  HostGlobals,
  HostGlobalValue,
  BridgeWithShim,
  RebindValue,
  RebindGlobals,
  Imports,
  ImportValue,
  HostModuleObject,
  HostModuleValue,
  HostExportData,
  HostExportFunction,
  RebindImports,
  RebindHostModule,
  CreateSandbox,
  Sandbox,
  SandboxOptions,
  PrecompileOptions,
  Prefix,
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

export async function createSandbox(options?: SandboxOptions): Promise<Sandbox> {
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
    process.stderr.write(`[@iso4/sandbox] failed to start V8 process: ${err.message}\n`)
  })

  await waitForSocket(socketPath)

  // Open all pool connections in parallel.
  const clients = await Promise.all(
    Array.from({ length: maxIsolates }, () =>
      RuntimeIpcClient.connect({ socketPath, token })),
  )

  const pool = new ConnectionPool(clients)
  return new SandboxImpl(proc, pool, socketPath)
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
    `[@iso4/sandbox] V8 process socket not available after ${timeoutMs}ms: ${socketPath}`,
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
  maxBridgeCallBytes: number
  maxBridgeCalls: number
} {
  return {
    memoryMb: limits?.memoryMb ?? 64,
    cpuTimeMs: limits?.cpuTimeMs ?? 5_000,
    wallTimeMs: limits?.wallTimeMs ?? 30_000,
    maxExportBytes: limits?.maxExportBytes ?? 16 * 1024 * 1024,
    maxStdoutBytes: limits?.maxStdoutBytes ?? 1 * 1024 * 1024,
    maxStderrBytes: limits?.maxStderrBytes ?? 1 * 1024 * 1024,
    maxBridgeCallBytes: limits?.maxBridgeCallBytes ?? 16 * 1024 * 1024,
    maxBridgeCalls: limits?.maxBridgeCalls ?? 10,
  }
}

// ── Bridge dispatcher factory ─────────────────────────────────────────────

// ── SandboxImpl ─────────────────────────────────────────────────────────────

class SandboxImpl implements Sandbox {
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
    if (options.signal?.aborted) {
      return { ok: false, error: { code: 'ERR_ABORTED', name: 'AbortError', message: 'run was aborted' }, stdout: [], stderr: [], durationMs: 0 }
    }
    const { bridgeGlobals, preamble } = processGlobals(options.globals ?? {})
    const { bindings, registry, shape: _shape } = processImports(options.imports)
    // Host-module function leaves are reached through the single
    // `__iso4_call` dispatcher global, which routes by handle ID. Install it
    // alongside the user globals when any function leaves exist.
    const allGlobals = registry.size > 0
      ? { ...bridgeGlobals, [BRIDGE_DISPATCH_GLOBAL]: createDispatchGlobal(registry) }
      : bridgeGlobals
    const code = preamble ? `${preamble}\n${options.code}` : options.code
    return this.pool.withClient(async (client) => {
      const raw = await client.runRawCode(code, {
        filename: options.filename,
        limits: toWireLimits(options.limits),
        globals: allGlobals,
        imports: bindings,
      })
      return decodeRunCompletionPayload(raw.result).result
    })
  }

  /**
   * @param options
   */
  async precompile<
    // eslint-disable-next-line @typescript-eslint/no-empty-object-type
    G extends HostGlobals = {},
    // eslint-disable-next-line @typescript-eslint/no-empty-object-type
    M extends Imports = {},
  >(
    options: PrecompileOptions<G, M>,
  ): Promise<Prefix<G, M>> {
    return this.pool.withClient(async (client) => {
      const rawGlobals = options.globals ?? {} as G
      const { bridgeGlobals, preamble } = processGlobals(rawGlobals)
      const { bindings, registry, shape } = processImports(options.imports)
      // See the comment in `run` — host-module function leaves reach the
      // bridge through the single `__iso4_call` dispatcher global.
      const allGlobals = registry.size > 0
        ? { ...bridgeGlobals, [BRIDGE_DISPATCH_GLOBAL]: createDispatchGlobal(registry) }
        : bridgeGlobals
      const code = preamble ? `${preamble}\n${options.code}` : options.code
      const raw = await client.precompile({
        code,
        filename: options.filename,
        limits: toWireLimits(options.limits),
        globals: allGlobals,
        imports: bindings,
      })
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

      return new PrefixImpl<G, M>(result.prefixId, this.pool, rawGlobals, registry, shape)
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
      // already removed or never created - that's fine
    }
  }

  async [Symbol.asyncDispose](): Promise<void> {
    await this.dispose()
  }
}

// ── PrefixImpl ───────────────────────────────────────────────────────────────

// eslint-disable-next-line @typescript-eslint/no-empty-object-type
class PrefixImpl<G extends HostGlobals = {}, M extends Imports = {}>
implements Prefix<G, M> {
  readonly id: string
  private readonly pool: ConnectionPool
  /**
   * Handlers declared at precompile time. Used as defaults for any run that
   * does not supply its own override, per DESIGN.md §11.4 "Rebinding rules":
   * "If the prefix declared a global the run doesn't supply, the
   * precompile-time implementation is reused."
   */
  private readonly defaultGlobals: G
  /**
   * Handle registry declared at precompile time: handle ID → host function.
   * Same rebinding rules as `defaultGlobals` — a `prefix.run()` may override
   * a subset of function leaves; everything else falls back to these. Source
   * modules and data leaves are frozen in the snapshot and not represented
   * here.
   */
  private readonly defaultRegistry: HandleRegistry
  /**
   * Declared shape captured at precompile time. Drives type-level rebinding
   * via `RebindImports<M>` and runtime `ERR_UNDECLARED_BINDING` enforcement
   * inside `extractRebindImports`.
   */
  private readonly declaredImportShape: DeclaredImportShape
  private _alive = true

  constructor(
    id: string,
    pool: ConnectionPool,
    defaultGlobals: G,
    defaultRegistry: HandleRegistry,
    declaredImportShape: DeclaredImportShape,
  ) {
    this.id = id
    this.pool = pool
    this.defaultGlobals = defaultGlobals
    this.defaultRegistry = defaultRegistry
    this.declaredImportShape = declaredImportShape
  }

  get alive(): boolean {
    return this._alive
  }

  async run(options: PrefixRunOptions<G, M>): Promise<RunResult> {
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

    if (options.signal?.aborted) {
      return { ok: false, error: { code: 'ERR_ABORTED', name: 'AbortError', message: 'run was aborted' }, stdout: [], stderr: [], durationMs: 0 }
    }
    // Extract bridge globals, routing shimmed overrides to their private keys.
    // The preamble is already compiled into the snapshot — not re-injected.
    const bridgeGlobals = extractBridgeGlobals(
      (options.globals ?? {}) as RebindGlobals<HostGlobals>,
      this.defaultGlobals as HostGlobals,
    )
    let registry: HandleRegistry
    try {
      registry = extractRebindImports(
        options.imports as unknown as Parameters<typeof extractRebindImports>[0],
        this.defaultRegistry,
        this.declaredImportShape,
      )
    } catch (e) {
      if (e instanceof UndeclaredImportBindingError) {
        // Symmetric with the Rust-side ERR_UNDECLARED_BINDING path for globals:
        // surface the error as a RunResult failure rather than a thrown promise.
        return {
          ok: false,
          error: { code: 'ERR_UNDECLARED_BINDING', name: 'Error', message: e.message },
          stdout: [],
          stderr: [],
          durationMs: 0,
        }
      }
      throw e
    }
    // Host-module function leaves reach the bridge through the single
    // `__iso4_call` dispatcher global, keyed by handle ID.
    const allGlobals = registry.size > 0
      ? { ...bridgeGlobals, [BRIDGE_DISPATCH_GLOBAL]: createDispatchGlobal(registry) }
      : bridgeGlobals
    return this.pool.withClient(async (client) => {
      const raw = await client.prefixRun({
        prefixId: this.id,
        code: options.code,
        filename: options.filename,
        limits: toWireLimits(options.limits),
        globals: allGlobals,
      })
      return decodeRunCompletionPayload(raw.result).result
    })
  }

  async dispose(): Promise<void> {
    if (!this._alive)
      return
    this._alive = false
    // Fire-and-forget - no response frame for DisposePrefix.
    await this.pool.withClient((client) => client.disposePrefix(this.id))
  }

  async [Symbol.asyncDispose](): Promise<void> {
    await this.dispose()
  }
}
