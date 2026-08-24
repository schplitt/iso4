/**
 * \@iso4/sandbox - public entry point.
 *
 * Spawns the Rust V8 subprocess, manages a pool of UDS connections (one per
 * isolate slot), and exposes the `Sandbox` + `Prefix` API.
 */

import { access, unlink } from 'node:fs/promises'
import { unlinkSync } from 'node:fs'
import { availableParallelism, tmpdir, totalmem } from 'node:os'
import { join } from 'node:path'
import { randomUUID } from 'node:crypto'
import process from 'node:process'
import { spawn } from 'node:child_process'
import type { ChildProcess } from 'node:child_process'

import { resolveRuntimeBinary } from './binary'
import { ProtocolDesyncError, RunAbortedError, RuntimeIpcClient } from './client'

import { ConnectionPool } from './pool'
import {
  decodePrecompileResultPayload,
  decodeRunCompletionPayload,
} from './ipc'
import type { CallPayload } from './ipc'
import type {
  CallResult,
  HostGlobals,
  PrecompileOptions,
  PrefixCallOptions,
  PrefixRunOptions,
  RebindGlobals,
  RebindImports,
  Prefix,
  ReadExportsOptions,
  ReadExportsResult,
  ResourceLimits,
  RunCallOptions,
  RunOptions,
  RunResult,
  Sandbox,
  SandboxOptions,
  SandboxStats,
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
import { materializeHostTypesInGlobals, serializeHostValue } from './v8-codec.js'

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
  CallResult,
  CallSuccess,
  CallTarget,
  CreateSandbox,
  Sandbox,
  SandboxOptions,
  SandboxStats,
  PrecompileOptions,
  Prefix,
  PrefixCallOptions,
  PrefixRunOptions,
  ReadExportsOptions,
  ReadExportsResult,
  RunCallOptions,
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
  const maxIsolates = options?.maxIsolates ?? availableParallelism()
  const warmBudgetBytes = resolveWarmBudgetBytes(options?.memoryBudgetMb)

  // Generate a per-process unique socket path and a cryptographically
  // random auth token. Both are passed as CLI args to the Rust binary so
  // no other process can predict the path or authenticate.
  const socketPath = join(tmpdir(), `iso4-v8-${process.pid}-${randomUUID().slice(0, 8)}.sock`)
  const token = randomUUID()

  // The runtime needs exactly one capacity fact: the warm budget in bytes,
  // the RSS mark it sheds against (#66). Concurrency is bounded by this
  // host's connection pool; there is no instance-count cap (celld's
  // stance — their resident ceiling defaults to unlimited).
  const proc = spawn(binaryPath, [
    '--socket',
    socketPath,
    '--token',
    token,
    '--warm-budget-bytes',
    String(warmBudgetBytes),
  ], {
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

  // From here to the `return`, this function is the only owner of the child.
  // A caller that gets an exception never receives a `Sandbox`, so it has
  // nothing to call `dispose()` on — every failure has to shut the child down
  // itself or the process outlives the call, holding its isolate heaps and its
  // socket file until someone kills it by hand. A service that retries
  // `createSandbox` would otherwise collect one of those per attempt.
  try {
    await waitForSocket(socketPath, proc)

    // Open all pool connections in parallel. `allSettled`, not `all`: `all`
    // rejects on the first failure while its siblings keep running, so the
    // connections that did open are left with no owner. Killing the child
    // below closes them anyway, but only after they have each held a thread in
    // it, and only if the kill lands.
    const connect = (): Promise<RuntimeIpcClient> =>
      RuntimeIpcClient.connect({ socketPath, token })
    const settled = await Promise.allSettled(
      Array.from({ length: maxIsolates }, () => connect()),
    )
    const clients: RuntimeIpcClient[] = []
    let firstFailure: { reason: unknown } | undefined
    for (const outcome of settled) {
      if (outcome.status === 'fulfilled')
        clients.push(outcome.value)
      else if (firstFailure === undefined)
        firstFailure = { reason: outcome.reason }
    }
    if (firstFailure !== undefined) {
      await Promise.all(clients.map((client) => client.dispose().catch(() => {})))
      throw firstFailure.reason
    }

    // These connections set the pool's capacity. Within it the pool reuses
    // idle connections and opens a replacement on demand for one that died,
    // rather than holding a fixed set of slots (see `pool.ts`).
    const pool = new ConnectionPool(clients, connect)

    // Dedicated control connection for `stats()` (#65): it never enters the
    // pool, so a capacity snapshot answers even while every run slot is busy —
    // exactly when it is most wanted.
    let statsClient: RuntimeIpcClient
    try {
      statsClient = await connect()
    } catch (error) {
      // The child is dealt with by the outer handler; the pool's connections
      // are this block's to release.
      await pool.dispose().catch(() => {})
      throw error
    }

    return new SandboxImpl(proc, pool, statsClient, socketPath, options?.memoryMb)
  } catch (error) {
    proc.kill()
    // The runtime leaves its socket file behind, and nothing else will remove
    // it now — `dispose()` is the only other place that does, and there is no
    // instance to call it on.
    await unlink(socketPath).catch(() => {})
    throw error
  }
}

/**
 * The warm budget in bytes — the ONE capacity fact the runtime needs
 * (#66, celld's model): the RSS mark it sheds against, `0` = disabled.
 * Independent of `memoryMb`: RSS is measured, not derived from per-isolate
 * caps, so an uncapped-heap sandbox is budgeted all the same.
 * @param memoryBudgetMb the explicit budget knob (`0` opts out of
 * watermarks entirely, like celld's `CELLD_MAX_RSS_MB=0`), or undefined
 * for the container-aware default
 */
function resolveWarmBudgetBytes(memoryBudgetMb: number | undefined): number {
  if (memoryBudgetMb !== undefined && !Number.isFinite(memoryBudgetMb)) {
    // Infinity/NaN would reach the child as `--warm-budget-bytes Infinity`,
    // kill it at arg parsing, and surface as an unrelated socket timeout.
    throw new TypeError(
      '[@iso4/sandbox] memoryBudgetMb must be a finite number of megabytes',
    )
  }
  const budgetMb = memoryBudgetMb ?? defaultMemoryBudgetMb()
  // Clamp both ends: negatives (a nonsense budget) to 0 = disabled, and
  // huge budgets to the JS safe-integer range — beyond it the byte math
  // rounds (wrong mark enforced) and ≥ 1e21 even stringifies to
  // exponential notation, which kills the child at arg parsing and
  // surfaces as an unrelated socket timeout.
  return Math.min(
    Math.max(0, Math.floor(budgetMb * 1024 * 1024)),
    Number.MAX_SAFE_INTEGER,
  )
}

/**
 * Default memory budget: what this process may use — container/cgroup-aware
 * via `process.constrainedMemory()` (`os.totalmem()` lies inside containers;
 * the fallback covers bare metal, where constrainedMemory reports 0) — minus
 * a safety net of max(512 MB, 25 %) for the Node host, the Rust runtime, and
 * the embedding service's own per-isolate state.
 *
 * Floored at 64 MB: on a host at or below the 512 MB safety net the
 * subtraction goes to zero or negative, and a zero DEFAULT would silently
 * disable the watermarks on exactly the memory-starved machines that need
 * them most (`memoryBudgetMb: 0` stays the only deliberate opt-out). The
 * floor makes such a host shed warmth aggressively instead — degraded,
 * never unprotected — and says so on stderr.
 */
function defaultMemoryBudgetMb(): number {
  // constrainedMemory() reports 0/undefined when there is no cgroup limit —
  // except on cgroup v1, where "unlimited" is a sentinel near 2^63 (seen on
  // GitHub Actions runners). Take the smaller of it and the host total
  // instead of trusting either alone: a real container limit is below the
  // host total, and the sentinel is above it.
  const constrained = process.constrainedMemory?.() || Number.POSITIVE_INFINITY
  const totalBytes = Math.min(constrained, totalmem())
  const totalMb = totalBytes / (1024 * 1024)
  const budgetMb = Math.floor(totalMb - Math.max(512, totalMb * 0.25))
  if (budgetMb < 64) {
    process.stderr.write(
      `[@iso4/sandbox] host memory (${Math.floor(totalMb)} MB) leaves no room `
      + `for a warm budget after the ${Math.max(512, Math.floor(totalMb * 0.25))} MB `
      + `safety net — flooring the budget at 64 MB (expect aggressive eviction); `
      + `set memoryBudgetMb explicitly to tune or 0 to disable\n`,
    )
    return 64
  }
  return budgetMb
}

// ── Helpers ────────────────────────────────────────────────────────────────

async function waitForSocket(
  socketPath: string,
  proc: ChildProcess,
  timeoutMs = 5_000,
): Promise<void> {
  const deadline = Date.now() + timeoutMs
  while (Date.now() < deadline) {
    try {
      await access(socketPath)
      return
    } catch {
      // A child that died at startup — bad argument, a socket it could not
      // bind — will never create the file, so waiting out the timeout and then
      // blaming the socket reports the symptom and hides the cause. Its stderr
      // is inherited, so the real complaint is already on the host's stderr;
      // name the exit here so the thrown error points at it.
      if (proc.exitCode !== null || proc.signalCode !== null) {
        throw new Error(
          `[@iso4/sandbox] the V8 process exited before its socket appeared `
          + `(exit code ${proc.exitCode}, signal ${proc.signalCode}). `
          + `Its own diagnostics are on this process's stderr.`,
        )
      }
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
/**
 * Synthesized result for a run displaced by a connection desync. The run never
 * reached the isolate, so unlike {@link abortedResult} there is no partial
 * telemetry to carry over — every field is empty by construction.
 * @param error
 */
function desyncResult(error: ProtocolDesyncError): RunResult {
  return {
    status: 'failed',
    ok: false,
    error: {
      code: 'ERR_PROTOCOL_DESYNC',
      name: error.name,
      message: error.message,
    },
    stdout: [],
    stderr: [],
    durationMs: 0,
    cpuTimeMs: 0,
    bridgeCalls: [],
  }
}

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
  /**
   * Control connection for `stats()` — outside the pool so a snapshot
   * answers even when every run slot is busy.
   */
  private readonly statsClient: RuntimeIpcClient
  private readonly socketPath: string
  /**
   * Uniform per-isolate heap cap (#64), set once at `createSandbox` and
   * injected into every frame's limits — the wire still carries `memoryMb`
   * per run, this is simply the only writer. `undefined` defers to the
   * runtime default (128 MB).
   */
  private readonly memoryMb: number | undefined
  /**
   * Last-resort cleanup registered on the host's `exit`, removed by
   * `dispose()` so a program that creates and disposes many sandboxes does not
   * accumulate listeners.
   */
  private readonly exitHook: () => void
  private _alive = true

  constructor(
    proc: ChildProcess,
    pool: ConnectionPool,
    statsClient: RuntimeIpcClient,
    socketPath: string,
    memoryMb?: number,
  ) {
    this.proc = proc
    this.pool = pool
    this.statsClient = statsClient
    this.socketPath = socketPath
    this.memoryMb = memoryMb
    proc.once('exit', () => {
      this._alive = false
    })
    // Nothing but `dispose()` stops the runtime, so a host that exits without
    // calling it leaves the process running, holding its isolate heaps and its
    // socket file with no owner. This is the backstop for forgetting: it runs
    // on a normal exit and on the way out of an uncaught exception, and both
    // steps are synchronous because nothing asynchronous runs during exit.
    //
    // It is a backstop, not a replacement. A host killed by a signal runs no
    // JavaScript at all, so the only complete answer is calling `dispose()`.
    this.exitHook = () => {
      this.proc.kill()
      try {
        unlinkSync(this.socketPath)
      } catch {
        // already gone, or never created
      }
    }
    process.once('exit', this.exitHook)
  }

  get alive(): boolean {
    return this._alive
  }

  async run(options: RunCallOptions): Promise<CallResult>
  async run(options: RunOptions): Promise<RunResult>
  async run(options: RunOptions & Partial<RunCallOptions>): Promise<RunResult | CallResult> {
    if (options.signal?.aborted) {
      return abortedResult(options.signal.reason)
    }
    if (options.limits !== undefined && 'memoryMb' in options.limits) {
      throw new TypeError(
        '[@iso4/sandbox] limits.memoryMb was removed: the heap cap is uniform '
        + 'per Runtime and baked into each isolate at creation (#64) — set it '
        + 'once via createSandbox({ memoryMb }) instead',
      )
    }
    const { defs, dispatch } = processGlobals(options.globals ?? {})
    // Drain any Request/Response body before the payload encoder, which is
    // synchronous. See materializeHostTypesInGlobals.
    await materializeHostTypesInGlobals(defs)
    // Host modules cross the wire as shape data; the runtime builds them
    // natively and dispatches function-leaf calls back here by
    // (specifier, path) through `handlers`.
    const { bindings, handlers } = processImports(options.imports)
    // Call args cross as one blob holding the argument array — the same
    // host-type-aware leg as bridge responses, so a real `Request` (whose
    // body drain is async) works at any depth.
    const call = options.call === undefined
      ? undefined
      : {
          exportPath: options.call.export,
          argsBlob: await serializeHostValue(options.call.args ?? []),
        }
    try {
      return await this.pool.withClient(async (client) => {
        // Every global is installed natively by the runtime, so user code
        // always starts at line 1.
        const raw = await client.runRawCode(options.code, {
          filename: options.filename,
          limits: { ...options.limits, memoryMb: this.memoryMb },
          globals: defs,
          dispatch,
          imports: bindings,
          importDispatch: handlers,
          signal: options.signal,
          call,
        })
        // `call` present ⇒ the value blob is the function's return value;
        // absent ⇒ the exports object. Never both.
        const decoded = call === undefined
          ? decodeRunCompletionPayload(raw.result).result
          : decodeRunCompletionPayload(raw.result, 'call').result
        // Graceful terminate (#36): a run whose signal aborted and whose Rust
        // Result carries ERR_ABORTED is a deliberate abort, not a failure —
        // remap to `status: 'aborted'` with the abort reason, keeping the real
        // telemetry Rust reported.
        if (options.signal?.aborted && decoded.status === 'failed' && decoded.error.code === 'ERR_ABORTED')
          return abortedResult(options.signal.reason, decoded)
        return decoded
      }, options.signal)
    } catch (error) {
      if (error instanceof RunAbortedError)
        return abortedResult(error.reason)
      if (error instanceof ProtocolDesyncError)
        return desyncResult(error)
      throw error
    }
  }

  /**
   * A capacity/usage snapshot from the runtime's registry merged with the
   * host pool's queue counter. See {@link Sandbox.stats}.
   */
  async stats(): Promise<SandboxStats> {
    const raw = await this.statsClient.stats()
    return {
      activeRuns: raw.oneoffRunning + raw.warmBusy,
      queueDepth: this.pool.queueDepth,
      warmInstances: raw.warmBusy + raw.warmIdle,
      idleInstances: raw.warmIdle,
      idleHeapBytes: raw.idleHeapBytes,
      budgetBytes: raw.warmBudgetBytes,
      rssBytes: raw.rssBytes,
      underPressure: raw.underPressure,
      prefixes: Object.fromEntries(
        raw.prefixes.map((p) => [p.prefixId, { idle: p.idle, busy: p.busy }]),
      ),
    }
  }

  /**
   * Load a module once and read its serializable exports — the deploy path.
   * Function exports are skipped and reported; failures reject, like
   * `prepare()`. See {@link Sandbox.readExports}.
   * @param options
   */
  async readExports(options: ReadExportsOptions): Promise<ReadExportsResult> {
    const result = await this.run({
      code: options.code,
      filename: options.filename,
      limits: options.limits,
    })
    if (result.status !== 'completed') {
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
    return { exports: result.exports, skippedExports: result.skippedExports }
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
   * Validate and prepare a prefix for repeated runs. See
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
        limits: { ...options.limits, memoryMb: this.memoryMb },
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

      return new PrefixImpl<G, M>(result.prefixId, this.pool, rawGlobals, handlers, this.memoryMb)
    })
  }

  async dispose(): Promise<void> {
    if (!this._alive)
      return
    this._alive = false
    process.removeListener('exit', this.exitHook)
    await this.pool.dispose()
    await this.statsClient.dispose()
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
   * declared prefix state and not represented here. The declared shape itself lives with
   * the prefix in the Rust runtime, which enforces `ERR_UNDECLARED_BINDING`
   * for rebind attempts outside it.
   */
  private readonly defaultImportHandlers: ImportHandlerMap
  /**
   * The sandbox-level uniform heap cap — see `SandboxImpl.memoryMb`. Warm
   * instances (#64) are created with this cap; per-run values no longer
   * exist (the cap is baked into the isolate at creation).
   */
  private readonly memoryMb: number | undefined
  private _alive = true

  constructor(
    id: string,
    pool: ConnectionPool,
    defaultGlobals: G,
    defaultImportHandlers: ImportHandlerMap,
    memoryMb?: number,
  ) {
    this.id = id
    this.pool = pool
    this.defaultGlobals = defaultGlobals
    this.defaultImportHandlers = defaultImportHandlers
    this.memoryMb = memoryMb
  }

  get alive(): boolean {
    return this._alive
  }

  /**
   * Execute dynamic code against this prefix. See
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
    const result = await this.dispatch(options, { code: options.code })
    return result as RunResult
  }

  /**
   * Call a function exported by this prefix's module — no postfix compiled.
   * See {@link Prefix.call}.
   * @param options
   */
  async call(options: PrefixCallOptions<G, M>): Promise<CallResult> {
    // Same host-type-aware args leg as SandboxImpl.run — a real `Request`
    // (async body drain) works at any depth, as one blob holding the array.
    const call = {
      exportPath: options.export,
      argsBlob: await serializeHostValue(options.args ?? []),
    }
    const result = await this.dispatch(options, { call })
    return result as CallResult
  }

  /**
   * Shared body of {@link run} and {@link call}: disposal/abort guards,
   * bridge-global extraction, import-rebind merging, and the PrefixRun round
   * trip. `payload` carries the postfix source or the call — exactly one, as
   * the wire demands; it also selects how the result blob decodes.
   * @param options
   * @param options.globals
   * @param options.imports
   * @param options.limits
   * @param options.signal
   * @param options.filename
   * @param payload
   */
  private async dispatch(
    options: {
      globals?: RebindGlobals<G>
      imports?: RebindImports<M>
      limits?: ResourceLimits
      signal?: AbortSignal
      filename?: string
    },
    payload: { code: string, call?: undefined } | { code?: undefined, call: CallPayload },
  ): Promise<RunResult | CallResult> {
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
    if (options.limits !== undefined && 'memoryMb' in options.limits) {
      throw new TypeError(
        '[@iso4/sandbox] limits.memoryMb was removed: the heap cap is uniform '
        + 'per Runtime and baked into each isolate at creation (#64) — set it '
        + 'once via createSandbox({ memoryMb }) instead',
      )
    }
    // Extract bridge globals, routing shimmed overrides to their private keys.
    // String/data globals and shim wrappers are already compiled into the
    // declared prefix state — only the bridge stubs they call are re-installed per run.
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
          code: payload.code,
          filename: options.filename,
          limits: { ...options.limits, memoryMb: this.memoryMb },
          globals: defs,
          dispatch,
          importRebinds: merged.rebinds,
          importDispatch: merged.handlers,
          signal: options.signal,
          call: payload.call,
        })
        // `call` present ⇒ the value blob is the function's return value;
        // absent ⇒ the exports object. Never both.
        const decoded = payload.call === undefined
          ? decodeRunCompletionPayload(raw.result).result
          : decodeRunCompletionPayload(raw.result, 'call').result
        // See the note in SandboxImpl.run — remap a graceful ERR_ABORTED Result
        // to `status: 'aborted'`, preserving the runtime's telemetry.
        if (options.signal?.aborted && decoded.status === 'failed' && decoded.error.code === 'ERR_ABORTED')
          return abortedResult(options.signal.reason, decoded)
        return decoded
      }, options.signal)
    } catch (error) {
      if (error instanceof RunAbortedError)
        return abortedResult(error.reason)
      if (error instanceof ProtocolDesyncError)
        return desyncResult(error)
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
