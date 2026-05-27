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
   * host bridge calls (globals, host imports) is excluded.
   * @default 5_000
   */
  cpuTimeMs: number

  /**
   * Hard wall-clock cap including async waits.
   * @default 30_000
   */
  wallTimeMs: number

  /**
   * Maximum bytes allowed in a single bridge call payload (arguments +
   * return value combined). Applies to all host-bridge calls — globals and
   * host imports alike.
   * @default 16 * 1024 * 1024
   */
  maxBridgePayloadBytes: number
}

// ─────────────────────────────────────────────────────────────────────────
// Globals (generic bridge — any non-reserved name is permitted)
// ─────────────────────────────────────────────────────────────────────────

/**
 * Host-provided globals. Any name not reserved by V8 or the runtime is
 * permitted. Each value becomes a bridge stub in the sandbox global object.
 *
 * Reserved names (must not be used): `console`, `URL`, `URLSearchParams`,
 * `TextEncoder`, `TextDecoder`, `crypto`, `Event`, `AbortController`,
 * `AbortSignal`.
 *
 * Common usage:
 * ```ts
 * globals: {
 *   fetch: myFetchHandler,   // \@iso4/fetch provides createSafeFetch()
 *   myTool: async (arg) => doSomething(arg),
 * }
 * ```
 *
 * `fetch` is not special — it goes through the same bridge path as any
 * other entry. See DESIGN.md §4.2 and §12.1.
 */
export type HostGlobals = Record<string, HostExportFunction>

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
