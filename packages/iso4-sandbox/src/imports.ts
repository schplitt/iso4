/**
 * Imports processing helpers — separated so they can be unit-tested without
 * spawning the V8 binary.
 *
 * Two flavors of import per DESIGN.md §4.3, discriminated by `typeof value`:
 *
 *   string  → source module: passed straight to V8 as ESM source.
 *   object  → host module:   walked recursively and lowered to a plain data
 *                            tree (`HostModuleNodePayload`). The Rust runtime
 *                            builds the module natively from that shape — the
 *                            client never generates sandbox source.
 *
 * Dispatch mechanism — location-addressed handlers:
 *
 *   Every function leaf is identified by its `(specifier, key segments)`
 *   location (a key may contain dots). The client keeps a handler map keyed by that location; the
 *   runtime assigns its own handle IDs (tree-walk order over the declared
 *   bindings), builds async trampolines into the module, and resolves each
 *   call back to the location before the `BridgeCall` frame is written — so
 *   frames arrive here as `targetKind = import` with the specifier and leaf
 *   path, and the handle IDs never leave the runtime.
 *
 * Rebinding rules (mirroring globals): only host-module function leaves can
 * be rebound, with a function. The client checks every location against the
 * declared shape before sending; the runtime repeats the check.
 */

import type { HostModuleNodePayload, ImportBindingPayload, ImportRebindPayload } from './ipc.js'
import { encodeImportKey } from './ipc.js'
import type {
  HostExportFunction,
  HostModuleObject,
  Imports,
} from './types.js'
import { isValidExportName } from './names.js'

// ─────────────────────────────────────────────────────────────────────────
// Public surface
// ─────────────────────────────────────────────────────────────────────────

/**
 * The reserved global name of the bridge dispatcher backing host-module
 * function leaves. The Rust runtime installs it automatically whenever the
 * declared imports contain a function leaf; user globals may not use this
 * name.
 */
export const BRIDGE_DISPATCH_GLOBAL = '__iso4_call'

/**
 * Deepest host-module shape node, as a path length from a top-level export.
 * A wire rule shared with the runtime (`docs/protocol.md` §5.2).
 */
export const MAX_HOST_MODULE_DEPTH = 64

/**
 * Handler lookup for host-module function leaves, keyed by
 * `importHandlerKey(specifier, path)`. Built at `processImports` time
 * (precompile / direct run) and re-derived with per-run overrides by
 * `mergeRebindImports` on `prefix.run()`.
 */
export type ImportHandlerMap = Map<string, HostExportFunction>

/**
 * The dispatch-map key for one host-module function leaf: the wire bytes of
 * its specifier and key segments, so `{ 'a.b': f }` and `{ a: { b: f } }` differ.
 * @param specifier
 * @param path the leaf's key segments
 */
export const importHandlerKey: (specifier: string, path: readonly string[]) => string = encodeImportKey

export interface ProcessedImports {
  /**
   * Wire-shaped binding list, one per specifier — source text for source
   * modules, a data tree for host modules.
   */
  bindings: ImportBindingPayload[]
  /**
   * Handlers for all host-module function leaves in this config, keyed by
   * `importHandlerKey(specifier, path)`.
   */
  handlers: ImportHandlerMap
}

export interface ProcessedRebinds {
  /**
   * Rebind locations to send on the `PrefixRun` payload — the runtime
   * validates them against the declared shape.
   */
  rebinds: ImportRebindPayload[]
  /**
   * Per-run handler map: the precompile defaults with the run's overrides
   * applied on top.
   */
  handlers: ImportHandlerMap
}

/**
 * Thrown for an import rebind the prefix does not allow. `PrefixImpl.run`
 * rethrows it as the same coded `Error` the runtime's refusals become.
 */
export class ImportRebindError extends Error {
  readonly code: 'ERR_UNDECLARED_BINDING' | 'ERR_FROZEN_BINDING' | 'ERR_INVALID_REBIND'
  constructor(code: 'ERR_UNDECLARED_BINDING' | 'ERR_FROZEN_BINDING' | 'ERR_INVALID_REBIND', message: string) {
    super(message)
    this.name = 'ImportRebindError'
    this.code = code
  }
}

// ─────────────────────────────────────────────────────────────────────────
// processImports
// ─────────────────────────────────────────────────────────────────────────

export function processImports(imports: Imports | undefined): ProcessedImports {
  if (imports === undefined)
    return { bindings: [], handlers: new Map() }

  const bindings: ImportBindingPayload[] = []
  const handlers: ImportHandlerMap = new Map()

  for (const [specifier, value] of Object.entries(imports)) {
    if (!specifier.isWellFormed())
      throw new TypeError(`[@iso4/sandbox] import specifier '${specifier}' is not a well-formed string`)
    if (typeof value === 'string') {
      // Source module: pass through verbatim.
      bindings.push({ specifier, source: value })
      continue
    }
    if (!isPlainObject(value)) {
      throw new Error(
        `[@iso4/sandbox] imports['${specifier}'] must be a string (source module) `
        + `or a plain object (host module); got ${describeKind(value)}`,
      )
    }
    // Host module: lower the object to a wire-shaped data tree and register
    // its function leaves in the handler map.
    const module: [string, HostModuleNodePayload][] = []
    for (const [name, child] of Object.entries(value)) {
      if (child === undefined)
        continue
      if (!isValidExportName(name)) {
        throw new Error(
          `[@iso4/sandbox] imports['${specifier}'] top-level key '${name}' is not a `
          + `valid JavaScript identifier and cannot be exported as a named ESM export`,
        )
      }
      module.push([name, lowerNode(specifier, child, [name], handlers)])
    }
    bindings.push({ specifier, module })
  }

  return { bindings, handlers }
}

/**
 * Lower one host-module node to its wire shape, registering function leaves.
 * @param specifier
 * @param value
 * @param path
 * @param handlers
 * @param seen
 */
function lowerNode(
  specifier: string,
  value: unknown,
  path: string[],
  handlers: ImportHandlerMap,
  seen: Set<object> = new Set(),
): HostModuleNodePayload {
  if (path.length > MAX_HOST_MODULE_DEPTH) {
    throw new Error(
      `[@iso4/sandbox] imports['${specifier}'].${path.join('.')}: `
      + `host-module shapes may nest at most ${MAX_HOST_MODULE_DEPTH} levels deep`,
    )
  }
  if (typeof value === 'function') {
    handlers.set(importHandlerKey(specifier, path), value as HostExportFunction)
    return { kind: 'function' }
  }
  if (isPlainObject(value)) {
    if (seen.has(value)) {
      throw new Error(
        `[@iso4/sandbox] imports['${specifier}'].${path.join('.')}: `
        + `circular references are not supported in host-module shapes`,
      )
    }
    seen.add(value)
    const entries: [string, HostModuleNodePayload][] = []
    for (const [key, child] of Object.entries(value)) {
      if (child === undefined)
        continue
      if (!key.isWellFormed())
        throw new TypeError(`[@iso4/sandbox] import key '${key}' is not a well-formed string`)
      entries.push([key, lowerNode(specifier, child, [...path, key], handlers, seen)])
    }
    seen.delete(value)
    return { kind: 'object', entries }
  }
  // Data leaf. Deliberately NOT inspected here: the value is handed straight to
  // V8's serializer, which is the single gate on what may cross (§4.2). Walking
  // the graph first would duplicate exactly what the serializer does, on the
  // Node main thread, at O(values) — the cost this package is trying to shed —
  // and any pre-walk would drift from V8's real capabilities over time.
  // Unsupported values therefore surface as the serializer's own data-clone
  // error at encode time rather than as a path-annotated error here.
  return { kind: 'data', value }
}

// ─────────────────────────────────────────────────────────────────────────
// mergeRebindImports — `prefix.run()` overrides
// ─────────────────────────────────────────────────────────────────────────

/**
 * The declared import shape without data-leaf values or source text, kept by a
 * prefix for {@link mergeRebindImports} so it does not pin large data.
 * @param bindings the bindings sent at `prepare()`
 */
export function declaredImportShape(bindings: readonly ImportBindingPayload[]): ImportBindingPayload[] {
  const strip = (entries: readonly (readonly [string, HostModuleNodePayload])[]): [string, HostModuleNodePayload][] =>
    entries.map(([key, node]) => [
      key,
      node.kind === 'object' ? { kind: 'object', entries: strip(node.entries) } : node.kind === 'data' ? { kind: 'data', value: undefined } : node,
    ])
  return bindings.map((b) => 'source' in b ? { specifier: b.specifier, source: '' } : { specifier: b.specifier, module: strip(b.module) })
}

/**
 * Merge the handlers behind a `Prefix`'s host-module function leaves with the
 * run's overrides and collect the rebind locations to send on the wire.
 *
 * Every location is looked up in the declared shape before its value is
 * looked at, with the same codes and messages as `validate_import_rebinds` in
 * the runtime, which repeats the check: undeclared is `ERR_UNDECLARED_BINDING`,
 * a source module or data leaf `ERR_FROZEN_BINDING`. A declared location
 * given the wrong kind of value is `ERR_INVALID_REBIND`.
 *
 * @param runImports the run's `imports` option
 * @param defaults handler map captured at precompile time
 * @param declared the import shape declared at `prepare()`
 */
export function mergeRebindImports(
  runImports: Imports | undefined,
  defaults: ImportHandlerMap,
  declared: readonly ImportBindingPayload[],
): ProcessedRebinds {
  if (runImports === undefined)
    return { rebinds: [], handlers: defaults }

  const handlers: ImportHandlerMap = new Map(defaults)
  const rebinds: ImportRebindPayload[] = []

  for (const [specifier, value] of Object.entries(runImports as Record<string, unknown>)) {
    if (!specifier.isWellFormed())
      throw new TypeError(`[@iso4/sandbox] import specifier '${specifier}' is not a well-formed string`)
    const binding = declared.find((b) => b.specifier === specifier)
    if (binding === undefined)
      throw new ImportRebindError('ERR_UNDECLARED_BINDING', `import '${specifier}' was not declared at precompile time`)
    if ('source' in binding) {
      throw new ImportRebindError(
        'ERR_FROZEN_BINDING',
        `import '${specifier}' is a source module — source imports are frozen with the prefix and cannot be rebound at prefix.run() time`,
      )
    }
    if (!isPlainObject(value)) {
      throw new ImportRebindError(
        'ERR_INVALID_REBIND',
        `import '${specifier}' can only be rebound with a plain object; got ${describeKind(value)}`,
      )
    }
    walkRebind(specifier, value, binding.module, [], handlers, rebinds)
  }

  return { rebinds, handlers }
}

function walkRebind(
  specifier: string,
  node: HostModuleObject,
  declared: readonly (readonly [string, HostModuleNodePayload])[],
  path: string[],
  handlers: ImportHandlerMap,
  rebinds: ImportRebindPayload[],
): void {
  for (const [key, value] of Object.entries(node)) {
    if (!key.isWellFormed())
      throw new TypeError(`[@iso4/sandbox] import key '${key}' is not a well-formed string`)
    const segments = [...path, key]
    const joined = segments.join('.')
    const shape = declared.find(([name]) => name === key)?.[1]
    if (shape === undefined)
      throw new ImportRebindError('ERR_UNDECLARED_BINDING', `import '${specifier}'.${joined} was not declared at precompile time`)
    if (shape.kind === 'data') {
      throw new ImportRebindError(
        'ERR_FROZEN_BINDING',
        `import '${specifier}'.${joined} is a data leaf, not a function — data leaves cannot be rebound`,
      )
    }
    if (shape.kind === 'object') {
      if (!isPlainObject(value)) {
        throw new ImportRebindError(
          'ERR_INVALID_REBIND',
          `import '${specifier}'.${joined} can only be rebound with a plain object; got ${describeKind(value)}`,
        )
      }
      walkRebind(specifier, value, shape.entries, segments, handlers, rebinds)
      continue
    }
    if (typeof value !== 'function') {
      throw new ImportRebindError(
        'ERR_INVALID_REBIND',
        `import '${specifier}'.${joined} can only be rebound with a function; got ${describeKind(value)}`,
      )
    }
    handlers.set(importHandlerKey(specifier, segments), value as HostExportFunction)
    rebinds.push({ specifier, path: segments })
  }
}

// ─────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────

function isPlainObject(value: unknown): value is HostModuleObject {
  if (value === null || typeof value !== 'object')
    return false
  if (Array.isArray(value))
    return false
  // Only a genuinely plain object describes nested host-module *shape*.
  // Everything else with an interesting prototype — `Date`, `Map`, `Set`,
  // `RegExp`, `Error`, `ArrayBuffer`, any `TypedArray`, class instances — is a
  // data leaf, handed to V8's serializer as-is. This is O(1) per node (it never
  // descends into a leaf), and the prototype check alone covers every one of
  // those types, so there is no per-type list here to keep in sync.
  const proto = Object.getPrototypeOf(value)
  return proto === null || proto === Object.prototype
}

function describeKind(value: unknown): string {
  if (value === null)
    return 'null'
  if (Array.isArray(value))
    return 'array'
  return typeof value
}
