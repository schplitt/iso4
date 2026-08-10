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
import { RunAbortedError, RuntimeIpcClient } from './client'

import { ConnectionPool } from './pool'
import {
  decodePrecompileResultPayload,
  decodeRunCompletionPayload,
} from './ipc'
import type {
  HostGlobals,
  PrecompileOptions,
  PrefixRunOptions,
  RebindGlobals,
  Prefix,
  RunOptions,
  RunResult,
  Sandbox,
  SandboxOptions,
  Imports,
} from './types'

// ── Global processing (imported from globals.ts for testability) ──────────

import { extractBridgeGlobals, processGlobals } from './globals.js'
import {
  UndeclaredImportBindingError,
  mergeRebindImports,
  processImports,
} from './imports.js'
import type { ImportHandlerMap } from './imports.js'
import { materializeHostTypesInGlobals } from './v8-codec.js'

export type {
  ResourceLimits,
  HostGlobals,
  HostGlobalValue,
  DataGlobal,
  BridgeCallEntry,
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
    // (the [iso4-v8] lines) appear in the host process's stderr.
    //
    // Per-run trace lines are off by default — writing two lines to an
    // inherited stderr costs 2-4 % of a hot run. Set `ISO4_V8_TRACE=1` in the
    // environment to re-enable them; errors and lifecycle events are always
    // logged.
    stdio: ['ignore', 'ignore', 'inherit'],
  })

  proc.once('error', (err) => {
    process.stderr.write(`[@iso4/sandbox] failed to start V8 process: ${err.message}\n`)
  })

  await waitForSocket(socketPath)

  // Open all pool connections in parallel.
  const connect = (): Promise<RuntimeIpcClient> =>
    RuntimeIpcClient.connect({ socketPath, token })
  const clients = await Promise.all(
    Array.from({ length: maxIsolates }, () => connect()),
  )

  // The pool uses `connect` to reopen any slot torn down by an in-flight abort,
  // keeping the pool at `maxIsolates`.
  const pool = new ConnectionPool(clients, connect)
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
 * The `RunResult` returned when a run is aborted — whether the signal was
 * already aborted at run entry or fired mid-flight. `error` (code `ERR_ABORTED`)
 * is retained for backward compatibility; `reason` carries whatever was passed
 * to `abort(reason)`.
 *
 * When graceful termination (#36) succeeds, Rust sends a real `ERR_ABORTED`
 * Result and `from` carries its telemetry — duration, CPU time, bridge records,
 * and any logs produced before the abort landed — which is grafted onto the
 * aborted shape. When it falls back to socket teardown (or the signal was
 * already aborted at entry), no Result arrives from Rust, so `from` is omitted
 * and timings/`bridgeCalls` report zeros/empty.
 * @param reason
 * @param from
 */
function abortedResult(reason?: unknown, from?: RunResult): RunResult {
  return {
    status: 'aborted',
    ok: false,
    error: { code: 'ERR_ABORTED', name: 'AbortError', message: 'run was aborted' },
    reason,
    stdout: from?.stdout ?? [],
    stderr: from?.stderr ?? [],
    durationMs: from?.durationMs ?? 0,
    cpuTimeMs: from?.cpuTimeMs ?? 0,
    bridgeCalls: from?.bridgeCalls ?? [],
  }
}

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
      return abortedResult(options.signal.reason)
    }
    const { defs, dispatch } = processGlobals(options.globals ?? {})
    // Drain any Request/Response body before the payload encoder, which is
    // synchronous. See materializeHostTypesInGlobals.
    await materializeHostTypesInGlobals(defs)
    // Host modules cross the wire as shape data; the runtime builds them
    // natively and dispatches function-leaf calls back here by
    // (specifier, path) through `handlers`.
    const { bindings, handlers } = processImports(options.imports)
    try {
      return await this.pool.withClient(async (client) => {
        // Every global is installed natively by the runtime, so user code
        // always starts at line 1.
        const raw = await client.runRawCode(options.code, {
          filename: options.filename,
          limits: options.limits,
          globals: defs,
          dispatch,
          imports: bindings,
          importDispatch: handlers,
          signal: options.signal,
        })
        const decoded = decodeRunCompletionPayload(raw.result).result
        // Graceful terminate (#36): a run whose signal aborted and whose Rust
        // Result carries ERR_ABORTED is a deliberate abort, not a failure —
        // remap to `status: 'aborted'` with the abort reason, keeping the real
        // telemetry Rust reported.
        if (options.signal?.aborted && decoded.status === 'failed' && decoded.error.code === 'ERR_ABORTED')
          return abortedResult(options.signal.reason, decoded)
        return decoded
      })
    } catch (error) {
      if (error instanceof RunAbortedError)
        return abortedResult(error.reason)
      throw error
    }
  }

  /**
   * @deprecated Renamed to {@link SandboxImpl.prepare}; kept as an alias.
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
    return this.prepare<G, M>(options)
  }

  /**
   * Pre-compile a prefix of code into a V8 startup snapshot. See
   * {@link Sandbox.prepare}.
   * @param options
   */
  async prepare<
    // eslint-disable-next-line @typescript-eslint/no-empty-object-type
    G extends HostGlobals = {},
    // eslint-disable-next-line @typescript-eslint/no-empty-object-type
    M extends Imports = {},
  >(
    options: PrecompileOptions<G, M>,
  ): Promise<Prefix<G, M>> {
    return this.pool.withClient(async (client) => {
      const rawGlobals = options.globals ?? {} as G
      const { defs } = processGlobals(rawGlobals)
      await materializeHostTypesInGlobals(defs)
      // The declared import shape travels as data and is stored with the
      // prefix on the Rust side — it is the runtime's reference for building
      // the modules on every run and for validating rebind attempts. The
      // handler map is kept here as the per-run dispatch defaults.
      const { bindings, handlers } = processImports(options.imports)
      const raw = await client.precompile({
        code: options.code,
        filename: options.filename,
        limits: options.limits,
        globals: defs,
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

      return new PrefixImpl<G, M>(result.prefixId, this.pool, rawGlobals, handlers)
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
   * Host-import function-leaf handlers declared at precompile time, keyed by
   * `(specifier, path)`. Same rebinding rules as `defaultGlobals` — a
   * `prefix.run()` may override a subset of function leaves; everything else
   * falls back to these. Source modules and data leaves are frozen in the
   * snapshot and not represented here. The declared shape itself lives with
   * the prefix in the Rust runtime, which enforces `ERR_UNDECLARED_BINDING`
   * for rebind attempts outside it.
   */
  private readonly defaultImportHandlers: ImportHandlerMap
  private _alive = true

  constructor(
    id: string,
    pool: ConnectionPool,
    defaultGlobals: G,
    defaultImportHandlers: ImportHandlerMap,
  ) {
    this.id = id
    this.pool = pool
    this.defaultGlobals = defaultGlobals
    this.defaultImportHandlers = defaultImportHandlers
  }

  get alive(): boolean {
    return this._alive
  }

  /**
   * Execute dynamic code against this prefix's snapshot. See
   * {@link Prefix.execute}.
   * @param options
   */
  async execute(options: PrefixRunOptions<G, M>): Promise<RunResult> {
    return this.run(options)
  }

  /**
   * @param options
   * @deprecated Renamed to {@link PrefixImpl.execute}; kept as an alias.
   */
  async run(options: PrefixRunOptions<G, M>): Promise<RunResult> {
    if (!this._alive) {
      return {
        status: 'failed',
        ok: false,
        error: {
          code: 'ERR_PREFIX_DISPOSED',
          name: 'Error',
          message: `prefix '${this.id}' has been disposed`,
        },
        stdout: [],
        stderr: [],
        durationMs: 0,
        cpuTimeMs: 0,
        bridgeCalls: [],
      }
    }

    if (options.signal?.aborted) {
      return abortedResult(options.signal.reason)
    }
    // Extract bridge globals, routing shimmed overrides to their private keys.
    // String/data globals and shim wrappers are already compiled into the
    // snapshot — only the bridge stubs they call are re-installed per run.
    const { defs, dispatch } = extractBridgeGlobals(
      (options.globals ?? {}) as RebindGlobals<HostGlobals>,
      this.defaultGlobals as HostGlobals,
    )
    // Merge run-time handler overrides over the precompile defaults and
    // collect the rebind locations for the wire. Declared-shape enforcement
    // happens in the Rust runtime against the stored prefix shape (the same
    // ERR_UNDECLARED_BINDING check that guards globals); only client-visible
    // shape problems throw here.
    let merged: ReturnType<typeof mergeRebindImports>
    try {
      merged = mergeRebindImports(
        options.imports as Imports | undefined,
        this.defaultImportHandlers,
      )
    } catch (e) {
      if (e instanceof UndeclaredImportBindingError) {
        // Symmetric with the Rust-side ERR_UNDECLARED_BINDING path:
        // surface the error as a RunResult failure rather than a thrown promise.
        return {
          status: 'failed',
          ok: false,
          error: { code: 'ERR_UNDECLARED_BINDING', name: 'Error', message: e.message },
          stdout: [],
          stderr: [],
          durationMs: 0,
          cpuTimeMs: 0,
          bridgeCalls: [],
        }
      }
      throw e
    }
    try {
      return await this.pool.withClient(async (client) => {
        const raw = await client.prefixRun({
          prefixId: this.id,
          code: options.code,
          filename: options.filename,
          limits: options.limits,
          globals: defs,
          dispatch,
          importRebinds: merged.rebinds,
          importDispatch: merged.handlers,
          signal: options.signal,
        })
        const decoded = decodeRunCompletionPayload(raw.result).result
        // See the note in SandboxImpl.run — remap a graceful ERR_ABORTED Result
        // to `status: 'aborted'`, preserving the runtime's telemetry.
        if (options.signal?.aborted && decoded.status === 'failed' && decoded.error.code === 'ERR_ABORTED')
          return abortedResult(options.signal.reason, decoded)
        return decoded
      })
    } catch (error) {
      if (error instanceof RunAbortedError)
        return abortedResult(error.reason)
      throw error
    }
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
