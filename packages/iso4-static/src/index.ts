/**
 * @iso4/static — public entry point.
 *
 * In-process static runtime using napi-rs. Requires Docker/Kubernetes as
 * the outer security boundary. See DESIGN.md §1.2 and §13.
 *
 * NOT YET IMPLEMENTED — Phase 11. See DESIGN.md §9.
 */

import type { StaticRuntime, StaticRuntimeOptions } from './types'

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

  // @iso4/static-specific
  CreateStaticRuntime,
  StaticRuntime,
  StaticRuntimeOptions,
  StaticPrecompileOptions,
  StaticPrefix,
  CallResult,
  CallSuccess,
  CallFailure,
  CallError,
  CallErrorCode,
} from './types'

/**
 * Create a static runtime.
 *
 * NOTE: Not yet implemented. Calling this will throw.
 * See DESIGN.md §9 Phase 11 for the build plan.
 */
export async function createStaticRuntime(
  _options?: StaticRuntimeOptions,
): Promise<StaticRuntime> {
  throw new Error(
    '@iso4/static: createStaticRuntime() is not yet implemented. See DESIGN.md Phase 11.',
  )
}
