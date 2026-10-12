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
import { RUNTIME_OWNED_GLOBALS } from './names.js'
import type {
  BridgeGlobal,
  BridgeWithShim,
  DataGlobal,
  HostExportFunction,
  HostGlobalValue,
  HostGlobals,
  RebindGlobals,
  SetupErrorCode,
  StringGlobal,
} from './types.js'

/**
 * The error every setup refusal is thrown as, whichever side detects it: a
 * plain `Error` carrying the runtime's code.
 * @param code the setup code
 * @param message the runtime's message for it
 */
export function setupError(code: SetupErrorCode, message: string): Error & { code: SetupErrorCode } {
  const err = new Error(message) as Error & { code: SetupErrorCode }
  // Own `name` and `code`, exactly as `thrownRunError` builds the runtime's refusals.
  err.name = 'Error'
  err.code = code
  return err
}

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

export function isBridgeGlobal(v: HostGlobalValue): v is BridgeGlobal {
  return (
    typeof v === 'object'
    && v !== null
    && (v as BridgeGlobal).kind === 'bridge'
  )
}

export function isStringGlobal(v: HostGlobalValue): v is StringGlobal {
  return (
    typeof v === 'object'
    && v !== null
    && (v as StringGlobal).kind === 'string'
  )
}

/**
 * Private bridge-dispatch name for a `BridgeWithShim` handler. This is a wire
 * dispatch key and an object-property key — never interpolated into generated
 * code — so any global name is safe. The runtime recognises the
 * `__iso4_<name>_h` convention (`public_record_name` in `v8.rs`) and reports
 * bridge records under the public `<name>`.
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
 * map. See the module comment for the split. Throws a `TypeError` for a name
 * that is not a well-formed string and `ERR_RESERVED_NAME` for a runtime-owned
 * one.
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
    if (!name.isWellFormed())
      throw new TypeError(`[@iso4/sandbox] global name '${name}' is not a well-formed string`)
    if (RUNTIME_OWNED_GLOBALS.has(name))
      throw setupError('ERR_RESERVED_NAME', `global '${name}' is reserved by the runtime and cannot be provided by the host`)
    // The bare shorthands are always enumerable by design; the object forms
    // carry an optional opt-out (default true).
    if (typeof value === 'function') {
      defs.push({ kind: 'bridge', name, enumerable: true })
      dispatch[name] = value
    } else if (typeof value === 'string') {
      defs.push({ kind: 'string', name, expr: value, enumerable: true })
    } else if (isBridgeGlobal(value)) {
      defs.push({ kind: 'bridge', name, enumerable: value.enumerable ?? true })
      dispatch[name] = value.handler as HostExportFunction
    } else if (isStringGlobal(value)) {
      defs.push({ kind: 'string', name, expr: value.expr, enumerable: value.enumerable ?? true })
    } else if (isDataGlobal(value)) {
      defs.push({ kind: 'data', name, value: value.value, enumerable: value.enumerable ?? true })
    } else if (isBridgeWithShim(value)) {
      const handlerName = shimHandlerName(name)
      defs.push({
        kind: 'shim',
        name,
        shim: value.shim,
        handlerName,
        enumerable: value.enumerable ?? true,
      })
      dispatch[handlerName] = value.handler as HostExportFunction
    }
  }

  return { defs, dispatch }
}

/**
 * What a prefix keeps of one declared global: its kind, and for a function
 * global the default handler. Data values and source text are not kept.
 */
export type DeclaredGlobal
  = | { kind: 'value' }
    | { kind: 'bridge', handler: HostExportFunction, enumerable: boolean }
    | { kind: 'shim', handler: HostExportFunction }

/**
 * Copy the declared global shape at `prepare()`, so later changes to the
 * caller's object change nothing. Kinds `processGlobals` ignores are left out.
 * @param globals the `globals` passed to `prepare()`
 */
export function declareGlobals(globals: HostGlobals): Map<string, DeclaredGlobal> {
  const declared = new Map<string, DeclaredGlobal>()
  for (const [name, value] of Object.entries(globals)) {
    if (typeof value === 'function')
      declared.set(name, { kind: 'bridge', handler: value, enumerable: true })
    else if (typeof value === 'string' || isStringGlobal(value) || isDataGlobal(value))
      declared.set(name, { kind: 'value' })
    else if (isBridgeWithShim(value))
      declared.set(name, { kind: 'shim', handler: value.handler as HostExportFunction })
    else if (isBridgeGlobal(value))
      declared.set(name, { kind: 'bridge', handler: value.handler as HostExportFunction, enumerable: value.enumerable ?? true })
  }
  return declared
}

/**
 * Resolve bridge globals for a `prefix.run()` call.
 *
 * Value globals and shim wrappers are replayed from the declared prefix
 * state, so every returned def is `kind: 'bridge'`; a shim override is routed
 * to its private `__iso4_<name>_h` key. Throws the setup error the runtime
 * would send for an undeclared or frozen name, and `ERR_INVALID_REBIND` for a
 * non-function override.
 * @param runGlobals the run's `globals` option
 * @param declared the shape kept from `prepare()` by {@link declareGlobals}
 */
export function extractBridgeGlobals(
  runGlobals: RebindGlobals<HostGlobals>,
  declared: ReadonlyMap<string, DeclaredGlobal>,
): ProcessedGlobals {
  // Same checks and messages as the runtime's PrefixRun binding check.
  const overrides = runGlobals as Record<string, unknown>
  for (const [name, override] of Object.entries(overrides)) {
    if (!name.isWellFormed())
      throw new TypeError(`[@iso4/sandbox] global name '${name}' is not a well-formed string`)
    const shape = declared.get(name)
    if (shape === undefined)
      throw setupError('ERR_UNDECLARED_BINDING', `global '${name}' was not declared at precompile time`)
    if (shape.kind === 'value')
      throw setupError('ERR_FROZEN_BINDING', `global '${name}' is a value global, frozen with the prefix — only function globals can be rebound`)
    // A supplied non-function must not fall back to the (often broader) default.
    if (typeof override !== 'function')
      throw setupError('ERR_INVALID_REBIND', `global '${name}' can only be rebound with a function; got ${override === null ? 'null' : typeof override}`)
  }

  const defs: GlobalDefPayload[] = []
  const dispatch: Record<string, HostExportFunction> = {}
  for (const [name, shape] of declared) {
    if (shape.kind === 'value')
      continue
    const override = Object.hasOwn(overrides, name) ? overrides[name] as HostExportFunction : undefined
    // The shim handler stub is runtime-internal and non-enumerable regardless.
    const stub = shape.kind === 'shim' ? shimHandlerName(name) : name
    defs.push({ kind: 'bridge', name: stub, enumerable: shape.kind === 'shim' || shape.enumerable })
    dispatch[stub] = override ?? shape.handler
  }

  return { defs, dispatch }
}
