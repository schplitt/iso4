/**
 * iso4 — public entry point.
 *
 * This package is in early development. The full API surface is declared
 * in `./types.ts` (the source of truth for what the runtime will expose)
 * but most of the implementation is not yet wired up.
 *
 * See ../../DESIGN.md and ../../MONOREPO.md for the architectural plan.
 */

import type { Runtime, RuntimeOptions } from './types';

export type {
  // Runtime + lifecycle
  CreateRuntime,
  Runtime,
  RuntimeOptions,

  // Per-run input
  Run,
  RunOptions,

  // Precompiled prefixes
  PrecompileOptions,
  PrecompiledPrefix,
  PrefixRunOptions,

  // Limits
  ResourceLimits,

  // Globals (allowlisted)
  HostGlobals,
  FetchHandler,
  HostFetchRequest,
  HostFetchResponse,

  // Imports
  ImportsConfig,
  ImportDefinition,
  SourceImport,
  HostImport,
  HostExports,
  HostExportValue,
  HostExportData,
  HostExportFunction,

  // Result
  RunResult,
  RunSuccess,
  RunFailure,
  SandboxExports,
  RunError,
  RunErrorCode,
} from './types'

/**
 * Create a runtime.
 *
 * NOTE: Not yet implemented. Calling this will throw.
 * See DESIGN.md §9 for the phased build plan.
 */
export async function createRuntime(
  _options?: RuntimeOptions,
): Promise<Runtime> {
  throw new Error(
    'iso4: createRuntime() is not yet implemented. See DESIGN.md for the build plan.',
  )
}
