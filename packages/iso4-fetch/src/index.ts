/**
 * @iso4/fetch — public entry point.
 *
 * Hardened `FetchHandler` for the iso4 sandbox. The factory returns a
 * function the host plugs into `globals.fetch` when calling `precompile()`
 * or `run()`.
 *
 * See `../../../DESIGN.md` §12 for the threat model and `./types.ts` for
 * the option surface.
 *
 * STATUS: scaffolding. The factory currently returns a handler that
 * throws on every call. Implementation lands in build-plan phase 5
 * (see `../../../DESIGN.md` §9).
 */

import type { FetchHandler } from 'iso4'
import type { SafeFetchOptions } from './types.js'

export type {
  SafeFetchOptions,
  SafeFetchPolicy,
  SafeFetchRequest,
} from './types.js'

/**
 * Build a hardened `FetchHandler` driven by a host-supplied policy callback.
 *
 * The `policy` field is required. Every request flows through it, and the
 * host decides allow/deny on a per-request basis with access to the
 * canonical URL, method, headers, and (when `pinDns` is on) the resolved
 * destination IP.
 *
 * NOTE: Not yet implemented. The returned handler throws on every call.
 */
export function createSafeFetch(options: SafeFetchOptions): FetchHandler {
  // Capture options so the (future) implementation can read them. The
  // explicit reference keeps TypeScript's `noUnusedParameters` happy
  // while preserving the public signature.
  void options

  return async () => {
    throw new Error(
      '@iso4/fetch: createSafeFetch() is not yet implemented. See DESIGN.md §9 phase 5.',
    )
  }
}
