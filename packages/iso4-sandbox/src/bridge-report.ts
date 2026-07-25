/**
 * Bridge-call name resolution for `RunResult.bridgeCalls`.
 *
 * The records themselves are produced by the Rust runtime and arrive on the
 * Result frame (see `docs/protocol.md` §5.6) — the client's only job is
 * mapping wire-level stub names to the public names sandbox code used. The
 * mapping knowledge (shim naming convention, import handle registry) only
 * exists on the TS side, which is why this half stays here until the
 * host-import build moves to Rust (#37).
 */

import type { DeclaredImportShape } from './imports.js'
import { BRIDGE_DISPATCH_GLOBAL } from './imports.js'

/**
 * Build the raw-name → public-name resolver for one run:
 *
 * - `__iso4_call` + handle ID → `<specifier>.<path>` from the declared
 *   import shape (e.g. `tools:search.query`).
 * - `__iso4_<name>_h` (BridgeWithShim private stub) → `<name>`.
 * - anything else (plain globals) → as-is.
 *
 * The handle map is built lazily on the first import call, so runs without
 * host-module imports pay nothing.
 * @param shape import shape declared at precompile/run time
 */
export function makeBridgeNameResolver(
  shape: DeclaredImportShape,
): (rawName: string, importHandleId: number | undefined) => string {
  let handleNames: Map<number, string> | undefined
  return (rawName, importHandleId) => {
    if (rawName === BRIDGE_DISPATCH_GLOBAL && importHandleId !== undefined) {
      if (handleNames === undefined) {
        handleNames = new Map()
        for (const [specifier, paths] of Object.entries(shape.hostFunctionIds)) {
          for (const [path, id] of Object.entries(paths))
            handleNames.set(id, `${specifier}.${path}`)
        }
      }
      return handleNames.get(importHandleId) ?? `${BRIDGE_DISPATCH_GLOBAL}#${importHandleId}`
    }
    if (rawName.startsWith('__iso4_') && rawName.endsWith('_h'))
      return rawName.slice('__iso4_'.length, -'_h'.length)
    return rawName
  }
}
