/**
 * \@iso4/dynamic — two-process dynamic runtime.
 *
 * Shared types (FetchHandler, ResourceLimits, ImportsConfig, …) live in
 * \@iso4/core and are re-exported from here for convenience so consumers
 * only need to import from one place.
 *
 * Execution model: precompile a prefix once into a V8 snapshot, then call
 * prefix.run({ code }) for each piece of dynamic code (e.g. agent-generated).
 * A fresh isolate is created from the snapshot per call. Multiple concurrent
 * callers each get their own pool slot and run in parallel.
 * See DESIGN.md §1 and §13.
 */

import type { HostGlobals, ImportsConfig, ResourceLimits } from '@iso4/core'

export type {
  ResourceLimits,
  HostGlobals,
  ImportsConfig,
  ImportDefinition,
  SourceImport,
  HostImport,
  HostExports,
  HostExportValue,
  HostExportData,
  HostExportFunction,
} from '@iso4/core'

// ─────────────────────────────────────────────────────────────────────────
// Runtime
// ─────────────────────────────────────────────────────────────────────────

export interface RuntimeOptions {
  /**
   * Maximum number of isolates (UDS connection slots) running concurrently.
   * The Runtime maintains a pool of this many connections to the Rust process;
   * each executes one run() at a time. Additional callers queue.
   *
   * Multiple MCP agents / concurrent callers each get their own slot and
   * run in parallel up to this limit.
   *
   * Defaults to the number of logical CPUs on the host machine.
   */
  maxIsolates?: number

  /**
   * Override the path to the Rust V8 binary.
   * Default: auto-detect from sibling \@iso4/v8-* platform packages.
   */
  binaryPath?: string
}

export interface Runtime {
  /**
   * Execute a piece of JavaScript in a fresh V8 isolate.
   * For repeated use with a shared context, prefer precompile() + prefix.run().
   */
  run: (options: RunOptions) => Promise<RunResult>

  /**
   * Pre-compile a prefix of code into a V8 startup snapshot.
   *
   * The type parameter `G` is inferred from the `globals` you pass and flows
   * into the returned `DynamicPrefix<G>`. This lets TypeScript enforce at
   * the call site that `prefix.run()` can only rebind names declared here:
   *
   * ```ts
   * const prefix = await runtime.precompile({
   *   globals: { fetch: myHandler, myTool: otherHandler },
   * })
   * // prefix: DynamicPrefix<{ fetch: ..., myTool: ... }>
   *
   * prefix.run({ globals: { fetch: newHandler } })          // ✅
   * prefix.run({ globals: { undeclared: someHandler } })    // ❌ TS error
   * ```
   *
   * Globals declared here serve as **default** implementations for all runs.
   * `prefix.run()` may override any subset; omitted names reuse the default.
   */
  // eslint-disable-next-line @typescript-eslint/no-empty-object-type
  precompile: <G extends HostGlobals = {}>(
    options: PrecompileOptions<G>,
  ) => Promise<DynamicPrefix<G>>

  /**
   * Tear down the Rust process. Outstanding prefixes are invalidated.
   */
  dispose: () => Promise<void>

  /**
   * Enables `await using runtime = await createRuntime()` syntax.
   */
  [Symbol.asyncDispose]: () => Promise<void>

  readonly alive: boolean
}

export type CreateRuntime = (options?: RuntimeOptions) => Promise<Runtime>

// ─────────────────────────────────────────────────────────────────────────
// Precompiled prefix
// ─────────────────────────────────────────────────────────────────────────

/**
 * Options for `runtime.precompile()`.
 *
 * `G` is the shape of globals this prefix declares. TypeScript infers it
 * from the `globals` field, so the returned `DynamicPrefix<G>` is typed to
 * allow only those names to be rebound in subsequent `prefix.run()` calls.
 *
 * When `globals` is omitted the type parameter defaults to `{}` (no declared
 * globals). Declare globals explicitly to allow rebinding at run time.
 */
export interface PrecompileOptions<G extends HostGlobals> {
  /**
   * ESM source code to evaluate before snapshotting. Top-level await works.
   *
   * @example
   *   import { search } from 'tools:search';
   *   globalThis.search = search;
   */
  code: string

  /**
   * Declares the globals that sandbox code will be able to call.
   *
   * This defines the **bridge surface shape** — the full set of names the
   * sandbox may invoke as globals. Names declared here:
   *
   * - Become bridge stubs in the V8 context on every `prefix.run()` call.
   * - Can be rebound per-run via `prefix.run({ globals })`.
   * - Cannot be supplemented at run time — a name not declared here will
   *   cause `ERR_UNDECLARED_BINDING` if passed to `prefix.run()`.
   *
   * The handlers provided here are the **default** implementations reused by
   * any `prefix.run()` call that does not override them.
   */
  globals?: G

  imports?: ImportsConfig

  /**
   * Resource limits applied to prefix code evaluation.
   *
   * **Important:** limits are NOT enforced during `precompile()`. Prefix code
   * is assumed to be developer-authored (the host application author), not
   * AI-agent-generated. The trust model treats prefix code the same way you
   * would treat application startup code — it runs once at precompile time
   * under the developer's control.
   *
   * **Currently a no-op placeholder.** The field is sent over the wire but
   * silently discarded — no default limits are stored or propagated to
   * `prefix.run()` calls. Pass limits directly to each `prefix.run()` call.
   *
   * Limits that DO apply to `prefix.run()` calls:
   * - `cpuTimeMs` — active V8 execution time only (async host-call wait excluded)
   * - `wallTimeMs` — hard wall-clock cap including async waits
   * - `memoryMb` — heap + ArrayBuffer budget (Phase 8, not yet enforced)
   */
  limits?: Partial<ResourceLimits>

  /**
   * Optional filename for stack traces. Default: "<prefix>".
   */
  filename?: string
}

/**
 * Handle to a precompiled prefix.
 *
 * `G` is the shape of globals declared at precompile time. `prefix.run()`
 * accepts only `Partial<G>` for its `globals` field — you may override any
 * declared global's implementation per run, but you cannot add names that
 * were not declared at precompile time. TypeScript will catch this at the
 * call site when G is a specific type.
 *
 * Run many dynamic code strings against the same warm snapshot state.
 */
export interface DynamicPrefix<G extends HostGlobals> {
  readonly id: string

  /**
   * Execute dynamic code against this prefix's snapshot state.
   *
   * Globals declared at precompile time are active. You may rebind any subset
   * of them via `options.globals`; the precompile-time handler is used for
   * any name not overridden here.
   *
   * Typical pattern — dev wraps agent-generated code before passing it:
   * @example
   *   const result = await prefix.run({
   *     code: `export default await (${agentFn})()`,
   *   })
   */
  run: (options: PrefixRunOptions<G>) => Promise<RunResult>

  /**
   * Release the snapshot. Subsequent run() calls reject. Idempotent.
   */
  dispose: () => Promise<void>

  /**
   * Enables `await using prefix = await runtime.precompile(...)` syntax.
   */
  [Symbol.asyncDispose]: () => Promise<void>

  readonly alive: boolean
}

/**
 * Options for `prefix.run()`.
 *
 * `G` is the declared-globals shape from the matching `precompile()` call.
 * The `globals` field is typed as `Partial<G>` — only names present in `G`
 * (i.e. declared at precompile time) may be overridden. Passing any other
 * name is a TypeScript error at compile time and `ERR_UNDECLARED_BINDING`
 * at runtime.
 *
 * Omitting a declared name reuses the precompile-time default implementation.
 */
export interface PrefixRunOptions<G extends HostGlobals> {
  /**
   * Dynamic ESM source code to compile and run against the prefix state.
   */
  code: string

  /**
   * Override implementations for a **subset** of globals declared at
   * precompile time.
   *
   * - Only keys in `G` (declared via `precompile({ globals })`) are accepted.
   * - Adding a name not declared at precompile time: TypeScript error + runtime
   *   `ERR_UNDECLARED_BINDING`.
   * - Omitted names use the precompile-time default handler.
   * - Functions in return values are currently dropped (planned: callable
   *   handles, see DESIGN.md §14).
   */
  globals?: Partial<G>

  /**
   * Rebind host-module imports declared at precompile time.
   * Source module imports are frozen in the snapshot and cannot be rebound.
   * (Phase 6/7 — not yet implemented.)
   */
  imports?: ImportsConfig

  limits?: Partial<ResourceLimits>
  signal?: AbortSignal
  filename?: string
}

// ─────────────────────────────────────────────────────────────────────────
// Direct run (no prefix)
// ─────────────────────────────────────────────────────────────────────────

export interface RunOptions {
  code: string
  limits?: Partial<ResourceLimits>
  globals?: HostGlobals
  imports?: ImportsConfig
  signal?: AbortSignal
  filename?: string
}

// ─────────────────────────────────────────────────────────────────────────
// Result
// ─────────────────────────────────────────────────────────────────────────

export type RunResult = RunSuccess | RunFailure

export interface RunSuccess {
  ok: true
  exports: SandboxExports
  stdout: string[]
  stderr: string[]
  durationMs: number
}

export interface RunFailure {
  ok: false
  error: RunError
  stdout: string[]
  stderr: string[]
  durationMs: number
}

export type SandboxExports = {
  /**
   * Value of `export default` in user code, or `undefined`.
   */
  default: unknown

}
& {
  /**
   * Named exports live directly on the export object.
   */
  [name: string]: unknown
}

export interface RunError {
  code: RunErrorCode
  name: string
  message: string
  stack?: string
}

export type RunErrorCode
  = | 'ERR_USER_CODE'
    | 'ERR_MEMORY_LIMIT'
    | 'ERR_CPU_TIMEOUT'
    | 'ERR_WALL_TIMEOUT'
    | 'ERR_ABORTED'
    | 'ERR_MODULE_NOT_FOUND'
    | 'ERR_COMPILE'
    | 'ERR_FUNCTION_ARGUMENT_NOT_SUPPORTED'
    | 'ERR_EXPORT_NOT_SERIALIZABLE'
    | 'ERR_EXPORT_TOO_LARGE'
    | 'ERR_EXPORT_UNRESOLVED_PROMISE'
    | 'ERR_HOST_BRIDGE'
    | 'ERR_BRIDGE_PAYLOAD_TOO_LARGE'
    | 'ERR_UNDECLARED_BINDING'
    | 'ERR_PREFIX_DISPOSED'
    | 'ERR_INTERNAL'
