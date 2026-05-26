/**
 * \@iso4/core — shared types for the iso4 runtime ecosystem.
 *
 * Used by \@iso4/dynamic, \@iso4/static, and \@iso4/fetch.
 * No runtime code lives here — types only.
 */

// ─────────────────────────────────────────────────────────────────────────
// Resource limits
// ─────────────────────────────────────────────────────────────────────────

export interface ResourceLimits {
  /**
   * Hard cap on memory the isolate can use, in megabytes.
   * Covers the V8 heap and all external `ArrayBuffer` allocations.
   * @default 128
   */
  memoryMb: number

  /**
   * Maximum *active* execution time in milliseconds. Time spent waiting on
   * host bridge calls (fetch, host imports) is excluded.
   * @default 5_000
   */
  cpuTimeMs: number

  /**
   * Hard wall-clock cap including async waits.
   * @default 30_000
   */
  wallTimeMs: number

  /**
   * Maximum bytes per `fetch` response body.
   * @default 16 * 1024 * 1024
   */
  maxFetchBodyBytes: number
}

// ─────────────────────────────────────────────────────────────────────────
// Fetch (shared — @iso4/fetch implements FetchHandler for both runtimes)
// ─────────────────────────────────────────────────────────────────────────

/**
 * Host-side fetch handler. Implement directly or use \@iso4/fetch.
 */
export type FetchHandler = (
  request: HostFetchRequest,
) => Promise<HostFetchResponse> | HostFetchResponse

export interface HostFetchRequest {
  url: string
  method: string
  /**
   * Lower-cased, deduped.
   */
  headers: Record<string, string>
  /**
   * null for bodyless methods. Strings are UTF-8.
   */
  body: Uint8Array | string | null
  signal: AbortSignal
}

export interface HostFetchResponse {
  status: number
  statusText?: string
  headers: Record<string, string>
  body: Uint8Array | string | null
}

// ─────────────────────────────────────────────────────────────────────────
// Globals allowlist (same shape for both runtimes)
// ─────────────────────────────────────────────────────────────────────────

/**
 * Host-providable globals. Only allowlisted names are accepted.
 * Fetch is allowed in both runtimes even if rarely useful in \@iso4/static.
 */
export interface HostGlobals {
  fetch?: FetchHandler
}

// ─────────────────────────────────────────────────────────────────────────
// Imports (same resolution model for both runtimes)
// ─────────────────────────────────────────────────────────────────────────

/**
 * Resolution order: static map → dynamic resolver → ERR_MODULE_NOT_FOUND.
 */
export interface ImportsConfig {
  static?: Record<string, ImportDefinition>
  resolve?: (
    specifier: string,
    importer: string | null,
  ) => Promise<ImportDefinition | null> | ImportDefinition | null
}

export type ImportDefinition = SourceImport | HostImport

/**
 * Host provides ESM source; V8 compiles and caches it in-isolate.
 */
export interface SourceImport {
  kind: 'source'
  source: string
}

/**
 * Host provides an object; each function becomes a bridge-call stub.
 */
export interface HostImport {
  kind: 'host'
  exports: HostExports
}

export interface HostExports {
  default?: HostExportValue
  [name: string]: HostExportValue | undefined
}

export type HostExportValue = HostExportData | HostExportFunction

export type HostExportData
  = | null
    | undefined
    | boolean
    | number
    | bigint
    | string
    | Uint8Array
    | HostExportData[]
    | { [key: string]: HostExportData }

export type HostExportFunction = (
  ...args: HostExportData[]
) => (Promise<HostExportData | void> | HostExportData | void)
