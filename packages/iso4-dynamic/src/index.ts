/**
 * \@iso4/dynamic — public entry point.
 *
 * Two-process dynamic runtime: a Rust V8 subprocess handles sandboxed
 * execution; this package provides the TypeScript host API.
 *
 * Shared types (FetchHandler, ResourceLimits, ImportsConfig, …) are
 * re-exported here from \@iso4/core so consumers only need one import.
 *
 * See DESIGN.md and MONOREPO.md for the architectural plan.
 */

import type { Runtime, RuntimeOptions } from './types'

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

/**
 * Create a dynamic runtime. Spawns the Rust V8 process.
 *
 * NOTE: Not yet implemented. Calling this will throw until the Rust→TS result
 * payload uses the real structured value protocol.
 * @param _options
 */
export async function createRuntime(
  _options?: RuntimeOptions,
): Promise<Runtime> {
  throw new Error(
    '@iso4/dynamic: createRuntime() is not yet implemented. See DESIGN.md for the build plan.',
  )
}
