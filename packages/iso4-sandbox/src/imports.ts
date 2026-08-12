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
 *                            client never generates sandbox source (#37).
 *
 * Dispatch mechanism — location-addressed handlers:
 *
 *   Every function leaf is identified by its `(specifier, dot-joined path)`
 *   location. The client keeps a handler map keyed by that location; the
 *   runtime assigns its own handle IDs (tree-walk order over the declared
 *   bindings), builds async trampolines into the module, and resolves each
 *   call back to the location before the `BridgeCall` frame is written — so
 *   frames arrive here as `targetKind = import` with the specifier and leaf
 *   path, and the handle IDs never leave the runtime.
 *
 * Rebinding rules (mirroring globals):
 *   - Source modules cannot be rebound at `prefix.run()` (frozen at declaration).
 *   - Host-module function leaves can be rebound by passing a new function at
 *     the same path; data leaves cannot (frozen at declaration).
 *   - Declared-shape enforcement lives in the Rust runtime: `prefix.run()`
 *     sends the rebind locations and the runtime rejects anything that was
 *     not declared as a function leaf with `ERR_UNDECLARED_BINDING`, the same
 *     enforcement point that guards undeclared globals.
 */

import type { HostModuleNodePayload, ImportBindingPayload, ImportRebindPayload } from './ipc.js'
import type {
  HostExportFunction,
  HostModuleObject,
  Imports,
} from './types.js'

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
 * Handler lookup for host-module function leaves, keyed by
 * `importHandlerKey(specifier, path)`. Built at `processImports` time
 * (precompile / direct run) and re-derived with per-run overrides by
 * `mergeRebindImports` on `prefix.run()`.
 */
export type ImportHandlerMap = Map<string, HostExportFunction>

/**
 * The dispatch-map key for one host-module function leaf. The NUL separator
 * cannot collide with anything a specifier or dot-joined path can contain in
 * practice, so keys stay unambiguous.
 * @param specifier
 * @param path dot-joined function-leaf path
 */
export function importHandlerKey(specifier: string, path: string): string {
  return `${specifier}\u0000${path}`
}

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
 * Thrown when the host's import configuration is structurally invalid in a
 * way only the client can see (a non-function rebind value, a source-module
 * rebind attempt). `PrefixImpl.run` converts it into an
 * `ERR_UNDECLARED_BINDING` `RunResult`, matching the runtime-side enforcement
 * for undeclared locations.
 */
export class UndeclaredImportBindingError extends Error {
  readonly code = 'ERR_UNDECLARED_BINDING'
  constructor(message: string) {
    super(message)
    this.name = 'UndeclaredImportBindingError'
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
      if (!isValidExportIdentifier(name)) {
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
  if (typeof value === 'function') {
    handlers.set(importHandlerKey(specifier, path.join('.')), value as HostExportFunction)
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
 * Merge the handlers behind a `Prefix`'s host-module function leaves with the
 * run's overrides and collect the rebind locations to send on the wire.
 *
 * Only client-visible shape problems throw here (a string value — source
 * modules are frozen; a non-function leaf value — there is nothing to
 * dispatch to). Whether each location was actually declared as a function
 * leaf at precompile time is enforced by the Rust runtime against the stored
 * prefix shape, which rejects violations with `ERR_UNDECLARED_BINDING`.
 *
 * @param runImports
 * @param defaults handler map captured at precompile time
 */
export function mergeRebindImports(
  runImports: Imports | undefined,
  defaults: ImportHandlerMap,
): ProcessedRebinds {
  if (runImports === undefined)
    return { rebinds: [], handlers: defaults }

  const handlers: ImportHandlerMap = new Map(defaults)
  const rebinds: ImportRebindPayload[] = []

  for (const [specifier, value] of Object.entries(runImports)) {
    if (typeof value === 'string') {
      throw new UndeclaredImportBindingError(
        `import '${specifier}' is a source module — source imports are frozen `
        + `with the prefix and cannot be rebound at prefix.run() time`,
      )
    }
    if (!isPlainObject(value)) {
      throw new Error(
        `[@iso4/sandbox] imports['${specifier}'] override must be a plain object; `
        + `got ${describeKind(value)}`,
      )
    }
    walkRebind(specifier, value, [], handlers, rebinds)
  }

  return { rebinds, handlers }
}

function walkRebind(
  specifier: string,
  node: HostModuleObject,
  path: string[],
  handlers: ImportHandlerMap,
  rebinds: ImportRebindPayload[],
): void {
  for (const [key, value] of Object.entries(node)) {
    if (value === undefined)
      continue
    const newPath = [...path, key]
    if (typeof value === 'function') {
      const joined = newPath.join('.')
      handlers.set(importHandlerKey(specifier, joined), value as HostExportFunction)
      rebinds.push({ specifier, path: joined })
      continue
    }
    if (isPlainObject(value)) {
      walkRebind(specifier, value, newPath, handlers, rebinds)
      continue
    }
    throw new UndeclaredImportBindingError(
      `import '${specifier}'.${newPath.join('.')} can only be rebound with a `
      + `function; got ${describeKind(value)}`,
    )
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

const RESERVED_WORDS = new Set([
  'break',
  'case',
  'catch',
  'class',
  'const',
  'continue',
  'debugger',
  'default',
  'delete',
  'do',
  'else',
  'export',
  'extends',
  'finally',
  'for',
  'function',
  'if',
  'import',
  'in',
  'instanceof',
  'new',
  'return',
  'super',
  'switch',
  'this',
  'throw',
  'try',
  'typeof',
  'var',
  'void',
  'while',
  'with',
  'yield',
  'let',
  'static',
  'enum',
  'await',
  'implements',
  'package',
  'protected',
  'interface',
  'private',
  'public',
  'null',
  'true',
  'false',
])

/**
 * Whether `name` can appear as a named ESM export. JS identifier rules plus
 * a reserved-word filter. `default` is allowed (the runtime emits it as
 * `export default`). Mirrored by `is_valid_export_identifier` in the Rust
 * runtime (`v8.rs`), which owns the final defensive check.
 * @param name
 */
function isValidExportIdentifier(name: string): boolean {
  if (name === 'default')
    return true
  if (!/^[A-Za-z_$][A-Za-z0-9_$]*$/.test(name))
    return false
  if (RESERVED_WORDS.has(name))
    return false
  return true
}

function describeKind(value: unknown): string {
  if (value === null)
    return 'null'
  if (Array.isArray(value))
    return 'array'
  return typeof value
}
