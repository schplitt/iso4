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
   * The returned DynamicPrefix boots isolates from the snapshot on each run().
   *
   * globals and imports declared here define the bridge surface shape.
   * Subsequent prefix.run() calls rebind implementations but cannot add names.
   */
  precompile: (options: PrecompileOptions) => Promise<DynamicPrefix>

  /**
   * Tear down the Rust process. Outstanding prefixes are invalidated.
   */
  dispose: () => Promise<void>

  readonly alive: boolean
}

export type CreateRuntime = (options?: RuntimeOptions) => Promise<Runtime>

// ─────────────────────────────────────────────────────────────────────────
// Precompiled prefix
// ─────────────────────────────────────────────────────────────────────────

export interface PrecompileOptions {
  /**
   * ESM source code to evaluate before snapshotting. Top-level await works.
   *
   * @example
   *   import { search } from 'tools:search';
   *   globalThis.search = search;
   */
  code: string

  globals?: HostGlobals
  imports?: ImportsConfig
  limits?: Partial<ResourceLimits>

  /**
   * Optional filename for stack traces. Default: "<prefix>".
   */
  filename?: string
}

/**
 * Handle to a precompiled prefix. Run many dynamic code strings against
 * the same warm snapshot state.
 */
export interface DynamicPrefix {
  readonly id: string

  /**
   * Execute dynamic code against this prefix's snapshot state.
   *
   * Typical pattern — dev wraps agent-generated code before passing it:
   * @example
   *   const result = await prefix.run({
   *     code: `export default await (${agentFn})()`,
   *   })
   */
  run: (options: PrefixRunOptions) => Promise<RunResult>

  /**
   * Release the snapshot. Subsequent run() calls reject. Idempotent.
   */
  dispose: () => Promise<void>

  readonly alive: boolean
}

export interface PrefixRunOptions {
  /**
   * Dynamic ESM source code to compile and run against the prefix state.
   */
  code: string

  /**
   * Per-run implementations for globals declared at precompile time.
   * Names not declared at precompile time fail with ERR_UNDECLARED_BINDING.
   */
  globals?: HostGlobals

  /**
   * Per-run implementations for host-module imports declared at precompile time.
   * Source modules cannot be rebound (frozen in snapshot).
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
  stdout: string
  stderr: string
  durationMs: number
}

export interface RunFailure {
  ok: false
  error: RunError
  stdout: string
  stderr: string
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
   * Other named exports.
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
    | 'ERR_UNDECLARED_BINDING'
    | 'ERR_PREFIX_DISPOSED'
    | 'ERR_INTERNAL'
