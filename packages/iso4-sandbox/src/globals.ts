/**
 * Global processing helpers — separated so they can be unit-tested without
 * spawning the V8 binary.
 */

import type {
  BridgeWithShim,
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

/**
 * Split a `HostGlobals` map into:
 * - `bridgeGlobals`: plain functions sent to Rust as bridge stubs.
 *   - `HostExportFunction` → registered under its own name.
 *   - `BridgeWithShim` handler → registered under `__iso4_<name>_h`.
 * - `preamble`: JS source prepended to the user's code.
 *   - `string` → `globalThis["name"] = (<expr>)`
 *   - `BridgeWithShim` → wrapper that calls the private stub and passes the
 *     result through the shim: `async (...args) => await (shim)(await __iso4_<name>_h(...args))`
 * @param globals
 */
export function processGlobals(globals: HostGlobals): {
  bridgeGlobals: Record<string, HostExportFunction>
  preamble: string | undefined
} {
  const bridgeGlobals: Record<string, HostExportFunction> = {}
  const parts: string[] = []

  for (const [name, value] of Object.entries(globals)) {
    if (typeof value === 'function') {
      bridgeGlobals[name] = value
    } else if (typeof value === 'string') {
      parts.push(`globalThis[${JSON.stringify(name)}] = (${value})`)
    } else if (isBridgeWithShim(value)) {
      const privateKey = `__iso4_${name}_h`
      bridgeGlobals[privateKey] = value.handler as HostExportFunction
      parts.push(
        `globalThis[${JSON.stringify(name)}] = async (...args) => await (${value.shim})(await ${privateKey}(...args))`,
      )
    }
  }

  return {
    bridgeGlobals,
    preamble: parts.length > 0 ? parts.join('\n') : undefined,
  }
}

/**
 * Resolve bridge globals for a `prefix.run()` call.
 *
 * The preamble is already compiled into the snapshot — only the bridge
 * function pointers need resolving. Shimmed globals are routed to their
 * private `__iso4_<name>_h` key so rebinding updates the handler the shim
 * calls, without touching the shim itself.
 * @param runGlobals
 * @param precompileGlobals
 */
export function extractBridgeGlobals(
  runGlobals: RebindGlobals<HostGlobals>,
  precompileGlobals: HostGlobals,
): Record<string, HostExportFunction> {
  const out: Record<string, HostExportFunction> = {}

  for (const [name, value] of Object.entries(precompileGlobals)) {
    if (typeof value === 'string')
      continue // string globals are compiled into the snapshot; no bridge function

    if (isBridgeWithShim(value)) {
      // RebindValue<BridgeWithShim<H>> = H — override is always a function, never a new shim.
      const privateKey = `__iso4_${name}_h`
      const override = runGlobals[name]
      out[privateKey] = (typeof override === 'function'
        ? override
        : value.handler) as HostExportFunction
    } else if (typeof value === 'function') {
      const override = runGlobals[name]
      out[name] = (typeof override === 'function' ? override : value) as HostExportFunction
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
      out[name] = override as HostExportFunction
    }
  }

  return out
}
