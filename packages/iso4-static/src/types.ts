/**
 * \@iso4/static — in-process static runtime (Phase 11).
 *
 * napi-rs backend: runs inside the Node process using Node's existing V8
 * platform. No IPC overhead per call. Requires Docker/Kubernetes or
 * equivalent as the outer security boundary (no crash isolation).
 *
 * Execution model: precompile a prefix once into a V8 snapshot, then call
 * prefix.call('fnName', input) for each input value. The isolate is reused
 * across calls via an internal pool — no fresh-isolate overhead per call.
 * See DESIGN.md §1 and §13.
 *
 * NOT YET IMPLEMENTED. See DESIGN.md §9 Phase 11.
 */

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

export interface StaticRuntimeOptions {
  /**
   * Maximum number of live isolates in the pool per precompiled prefix.
   * Concurrent prefix.call() invocations each grab a slot; additional
   * callers queue until one is released.
   *
   * Defaults to the number of logical CPUs on the host machine.
   */
  maxConcurrent?: number
}

export interface StaticRuntime {
  /**
   * Pre-compile a prefix of code into a V8 snapshot and warm the isolate
   * pool. The returned StaticPrefix exposes prefix.call() for fast
   * repeated invocations.
   *
   * The prefix must export the functions that will be called via
   * prefix.call(). No dynamic code is accepted after this point.
   */
  precompile: (options: StaticPrecompileOptions) => Promise<StaticPrefix>

  /**
   * Dispose all live isolates and release the napi-rs addon resources.
   */
  dispose: () => Promise<void>

  readonly alive: boolean
}

export type CreateStaticRuntime = (
  options?: StaticRuntimeOptions,
) => Promise<StaticRuntime>

// ─────────────────────────────────────────────────────────────────────────
// Precompiled prefix
// ─────────────────────────────────────────────────────────────────────────

export interface StaticPrecompileOptions {
  /**
   * ESM source code to evaluate before snapshotting.
   * Must export the functions that will be called via prefix.call().
   *
   * @example
   *   import _ from 'lodash-es';
   *   export function transform(row) {
   *     return { revenue: row.price * row.qty };
   *   }
   */
  code: string

  globals?: import('@iso4/core').HostGlobals
  imports?: import('@iso4/core').ImportsConfig
  limits?: Partial<import('@iso4/core').ResourceLimits>
  filename?: string
}

/**
 * Handle to a precompiled static prefix. Call prefix.call() to invoke
 * an exported function with new input data. The isolate pool is managed
 * internally — no explicit open/close needed.
 */
export interface StaticPrefix {
  readonly id: string

  /**
   * Invoke a named export from the prefix with the given input.
   * Acquires a live isolate from the pool (or creates one), calls the
   * function, and returns the result. The isolate is returned to the pool.
   *
   * @example
   *   const result = await prefix.call('transform', { price: 10, qty: 3 })
   *   // → { ok: true, value: { revenue: 30 }, durationMs: 0.04 }
   *
   * @example parallel — all slots run concurrently up to maxConcurrent
   *   const results = await Promise.all(
   *     rows.map(row => prefix.call('transform', row))
   *   )
   */
  call: (fn: string, input?: unknown) => Promise<CallResult>

  /**
   * Release all pool isolates for this prefix. Idempotent.
   */
  dispose: () => Promise<void>

  readonly alive: boolean
}

// ─────────────────────────────────────────────────────────────────────────
// Result
// ─────────────────────────────────────────────────────────────────────────

export type CallResult = CallSuccess | CallFailure

export interface CallSuccess {
  ok: true
  /**
   * The function's return value, V8-deserialized.
   */
  value: unknown
  durationMs: number
}

export interface CallFailure {
  ok: false
  error: CallError
  durationMs: number
}

export interface CallError {
  code: CallErrorCode
  name: string
  message: string
  stack?: string
}

export type CallErrorCode
/**
 * The called function threw an uncaught exception.
 */
  = | 'ERR_USER_CODE'
	/**
	 * Memory limit exceeded.
	 */
    | 'ERR_MEMORY_LIMIT'
	/**
	 * CPU time limit exceeded.
	 */
    | 'ERR_CPU_TIMEOUT'
	/**
	 * Wall-clock limit exceeded.
	 */
    | 'ERR_WALL_TIMEOUT'
	/**
	 * Caller aborted via AbortSignal.
	 */
    | 'ERR_ABORTED'
	/**
	 * No export named `fn` found in the prefix.
	 */
    | 'ERR_FUNCTION_NOT_FOUND'
	/**
	 * The named export exists but is not a function.
	 */
    | 'ERR_NOT_A_FUNCTION'
	/**
	 * Input value could not be serialized for crossing the isolate boundary.
	 */
    | 'ERR_INPUT_NOT_SERIALIZABLE'
	/**
	 * Return value could not be deserialized.
	 */
    | 'ERR_RESULT_NOT_SERIALIZABLE'
	/**
	 * fetch was called but no handler was configured.
	 */
    | 'ERR_FETCH_NOT_CONFIGURED'
    | 'ERR_FETCH_BODY_TOO_LARGE'
    | 'ERR_FETCH_HOST'
    | 'ERR_FETCH_INVALID_HEADER'
    | 'ERR_FETCH_INVALID_URL'
	/**
	 * Prefix has been disposed.
	 */
    | 'ERR_PREFIX_DISPOSED'
    | 'ERR_INTERNAL'
