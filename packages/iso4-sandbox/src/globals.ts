/**
 * Global processing helpers — separated so they can be unit-tested without
 * spawning the V8 binary.
 *
 * Every global kind is installed natively by the runtime; the client generates
 * no sandbox source. `processGlobals` lowers the public `HostGlobals` map into
 * two things:
 *
 *   - `defs`: structured `GlobalDefPayload`s sent over the wire. Rust installs
 *     each one natively (bridge stub, evaluated expression, materialised
 *     constant, or shim wrapper) so user code always starts at line 1 and no
 *     global name is ever interpolated into an identifier position.
 *   - `dispatch`: the `name → handler` map the client routes incoming
 *     `BridgeCall`s through. Only bridge-backed globals appear here.
 */

import type { GlobalDefPayload } from './ipc.js'
import type {
  BridgeWithShim,
  DataGlobal,
  HostExportFunction,
  HostGlobalValue,
  HostGlobals,
  RebindGlobals,
} from './types.js'

export function isBridgeWithShim(v: HostGlobalValue): v is BridgeWithShim {
  return (
    typeof v === 'object'
    && v !== null
    && (v as BridgeWithShim).kind === 'bridge-with-shim'
  )
}

export function isDataGlobal(v: HostGlobalValue): v is DataGlobal {
  return (
    typeof v === 'object'
    && v !== null
    && (v as DataGlobal).kind === 'data'
  )
}

/**
 * Private bridge-dispatch name for a `BridgeWithShim` handler. This is a wire
 * dispatch key and an object-property key — never interpolated into generated
 * code — so any global name is safe. Kept as `__iso4_<name>_h` for backward
 * compatibility with the bridge-record name resolver (`bridge-report.ts`).
 * @param name public global name
 */
export function shimHandlerName(name: string): string {
  return `__iso4_${name}_h`
}

export interface ProcessedGlobals {
  /**
   * Structured global definitions to send over the wire. Rust installs each
   * one natively.
   */
  defs: GlobalDefPayload[]
  /**
   * Bridge-dispatch map: the wire-level stub name → host handler. Plain
   * functions register under their own name; shim handlers under their private
   * `__iso4_<name>_h` key. The client routes `BridgeCall` frames through this.
   */
  dispatch: Record<string, HostExportFunction>
}

/**
 * Lower a `HostGlobals` map into wire-shaped `defs` plus the bridge-dispatch
 * map. See the module comment for the split.
 *   - `HostExportFunction` → `{ kind: 'bridge' }` + dispatch entry.
 *   - `string`             → `{ kind: 'string', expr }` (evaluated by Rust).
 *   - `DataGlobal`         → `{ kind: 'data', value }` (materialised by Rust).
 *   - `BridgeWithShim`     → `{ kind: 'shim', shim, handlerName }` + a dispatch
 *     entry for the private handler under `handlerName`.
 * @param globals
 */
export function processGlobals(globals: HostGlobals): ProcessedGlobals {
  const defs: GlobalDefPayload[] = []
  const dispatch: Record<string, HostExportFunction> = {}

  for (const [name, value] of Object.entries(globals)) {
    if (typeof value === 'function') {
      defs.push({ kind: 'bridge', name })
      dispatch[name] = value
    } else if (typeof value === 'string') {
      defs.push({ kind: 'string', name, expr: value })
    } else if (isDataGlobal(value)) {
      defs.push({ kind: 'data', name, value: value.value })
    } else if (isBridgeWithShim(value)) {
      const handlerName = shimHandlerName(name)
      defs.push({ kind: 'shim', name, shim: value.shim, handlerName })
      dispatch[handlerName] = value.handler as HostExportFunction
    }
  }

  return { defs, dispatch }
}

/**
 * Resolve bridge globals for a `prefix.run()` call.
 *
 * String/data globals and shim *wrappers* are already compiled into the
 * snapshot — only the bridge *stubs* they call need re-installing per run
 * (bridge stubs are bound to the run's socket and are never snapshotted). So
 * every returned def is `kind: 'bridge'`, and shimmed globals are routed to
 * their private `__iso4_<name>_h` key so rebinding updates the handler the
 * shim calls, without touching the shim itself.
 * @param runGlobals
 * @param precompileGlobals
 */
export function extractBridgeGlobals(
  runGlobals: RebindGlobals<HostGlobals>,
  precompileGlobals: HostGlobals,
): ProcessedGlobals {
  const defs: GlobalDefPayload[] = []
  const dispatch: Record<string, HostExportFunction> = {}

  for (const [name, value] of Object.entries(precompileGlobals)) {
    if (typeof value === 'string' || isDataGlobal(value))
      continue // constants compiled into the snapshot; no bridge stub

    if (isBridgeWithShim(value)) {
      // RebindValue<BridgeWithShim<H>> = H — override is always a function, never a new shim.
      const handlerName = shimHandlerName(name)
      const override = runGlobals[name]
      defs.push({ kind: 'bridge', name: handlerName })
      dispatch[handlerName] = (typeof override === 'function'
        ? override
        : value.handler) as HostExportFunction
    } else if (typeof value === 'function') {
      const override = runGlobals[name]
      defs.push({ kind: 'bridge', name })
      dispatch[name] = (typeof override === 'function' ? override : value) as HostExportFunction
    }
  }

  // Pass any run-time override that was NOT in the precompile globals through
  // to Rust unchanged. Rust will reject it with ERR_UNDECLARED_BINDING, which
  // is the correct runtime behaviour for undeclared globals.
  // TypeScript's RebindGlobals<G> prevents this at the type level, but `as any`
  // can bypass it — Rust is the final enforcement point.
  for (const [name, override] of Object.entries(runGlobals)) {
    if (name in precompileGlobals)
      continue
    if (typeof override === 'function') {
      defs.push({ kind: 'bridge', name })
      dispatch[name] = override as HostExportFunction
    }
  }

  return { defs, dispatch }
}
