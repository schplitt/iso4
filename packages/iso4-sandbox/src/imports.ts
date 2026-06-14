/**
 * Imports processing helpers — separated so they can be unit-tested without
 * spawning the V8 binary.
 *
 * Two flavors of import per DESIGN.md §4.3, discriminated by `typeof value`:
 *
 *   string  → source module: passed straight to V8 as ESM source.
 *   object  → host module:   walked recursively; the runtime generates ESM
 *                            source that exposes the object's structure,
 *                            with function leaves replaced by async stubs.
 *
 * Dispatch mechanism — ID-addressed callable handles:
 *
 *   Every function leaf is assigned a small integer handle ID from a per-
 *   precompile counter and stored in a registry (`Map<id, fn>`). The
 *   generated source replaces each function with a stub:
 *
 *       (...args) => globalThis.__iso4_call(<id>, ...args)
 *
 *   `__iso4_call` is a single ordinary host global whose handler peels the
 *   leading ID and routes to the registry. This means:
 *     - No generated bridge-global *names* (no sanitisation, no collisions).
 *     - One global instead of N. The wire format and the Rust side are
 *       unchanged — `__iso4_call` is dispatched like any other global.
 *     - Deep nesting is free (the walker recurses; each leaf gets an ID).
 *     - Future request/response (functions returned from a bridge call) reuse
 *       the same registry + `__iso4_call` dispatcher — purely additive.
 *
 * Rebinding rules (mirroring globals):
 *   - Source modules cannot be rebound at `prefix.run()` (frozen in snapshot).
 *   - Host-module function leaves can be rebound by passing a new function at
 *     the same path; data leaves cannot (they are JS literals in the source).
 */

import type { ImportBindingPayload } from './ipc.js'
import type {
  HostExportData,
  HostExportFunction,
  HostModuleObject,
  HostModuleValue,
  Imports,
} from './types.js'

// ─────────────────────────────────────────────────────────────────────────
// Public surface
// ─────────────────────────────────────────────────────────────────────────

/**
 * The reserved global name of the bridge dispatcher. User globals may not
 * use this name. The generated host-module source calls it as
 * `globalThis.__iso4_call(id, ...args)`.
 */
export const BRIDGE_DISPATCH_GLOBAL = '__iso4_call'

/**
 * Maps handle IDs to host functions for one run. IDs are assigned at
 * `processImports` time (precompile) and reused for every `prefix.run()`
 * (the generated source baked into the snapshot references them by value).
 */
export type HandleRegistry = Map<number, HostExportFunction>

/**
 * The shape of imports declared at precompile time. Used by
 * `extractRebindImports` to reject any run-time override referencing an
 * undeclared specifier, path, or kind.
 */
export interface DeclaredImportShape {
  /**
   * Specifiers declared as source modules — never rebindable.
   */
  sourceSpecifiers: Set<string>
  /**
   * specifier → (dot-joined function-leaf path → handle ID). A path like
   * `"someObj.someMethod"` locates a function leaf in the host-module tree;
   * its ID is the registry key to overwrite when rebinding.
   */
  hostFunctionIds: Record<string, Record<string, number>>
  /**
   * specifier → set of dot-joined data-leaf paths. Tracked so we can produce
   * a precise error if the caller tries to rebind a data leaf.
   */
  hostDataPaths: Record<string, Set<string>>
}

export interface ProcessedImports {
  /**
   * Wire-shaped binding list, one per specifier; always source-form.
   */
  bindings: ImportBindingPayload[]
  /**
   * Handle registry for all host-module function leaves in this config.
   */
  registry: HandleRegistry
  /**
   * Declared shape captured for rebinding enforcement on `prefix.run()`.
   */
  shape: DeclaredImportShape
}

/**
 * Thrown when the host's import configuration is structurally invalid
 * (mixed types, unsupported data values, undeclared rebindings, …).
 * `PrefixImpl.run` converts the undeclared-binding variant into an
 * `ERR_UNDECLARED_BINDING` `RunResult`, matching the globals path.
 */
export class UndeclaredImportBindingError extends Error {
  readonly code = 'ERR_UNDECLARED_BINDING'
  constructor(message: string) {
    super(message)
    this.name = 'UndeclaredImportBindingError'
  }
}

/**
 * Build the `__iso4_call` global handler that backs all host-module
 * function leaves for a run. It peels the leading handle ID and routes to
 * the registry.
 * @param registry
 */
export function createDispatchGlobal(registry: HandleRegistry): HostExportFunction {
  return (...args: unknown[]): unknown => {
    const id = args[0] as number
    const fn = registry.get(id)
    if (fn === undefined)
      throw new Error(`[@iso4/sandbox] no host handler for import handle ${String(id)}`)
    return fn(...args.slice(1))
  }
}

// ─────────────────────────────────────────────────────────────────────────
// processImports
// ─────────────────────────────────────────────────────────────────────────

export function processImports(imports: Imports | undefined): ProcessedImports {
  const shape: DeclaredImportShape = {
    sourceSpecifiers: new Set(),
    hostFunctionIds: {},
    hostDataPaths: {},
  }
  if (imports === undefined)
    return { bindings: [], registry: new Map(), shape }

  const bindings: ImportBindingPayload[] = []
  const registry: HandleRegistry = new Map()
  let nextId = 0
  const allocId = (): number => nextId++

  for (const [specifier, value] of Object.entries(imports)) {
    if (typeof value === 'string') {
      // Source module: pass through verbatim.
      bindings.push({ specifier, source: value })
      shape.sourceSpecifiers.add(specifier)
      continue
    }
    if (typeof value !== 'object' || value === null || Array.isArray(value)) {
      throw new Error(
        `[@iso4/sandbox] imports['${specifier}'] must be a string (source module) `
        + `or a plain object (host module); got ${describeKind(value)}`,
      )
    }
    // Host module: walk the tree, build generated source + register handlers.
    const fnIds: Record<string, number> = {}
    const dataPaths = new Set<string>()
    const source = generateHostModuleSource(specifier, value, registry, allocId, fnIds, dataPaths)
    bindings.push({ specifier, source })
    shape.hostFunctionIds[specifier] = fnIds
    shape.hostDataPaths[specifier] = dataPaths
  }

  return { bindings, registry, shape }
}

// ─────────────────────────────────────────────────────────────────────────
// extractRebindImports — `prefix.run()` overrides
// ─────────────────────────────────────────────────────────────────────────

/**
 * Override the handlers behind a `Prefix`'s declared host-module function
 * leaves. Only declared paths may be rebound; everything else throws
 * `UndeclaredImportBindingError`. Source modules are frozen.
 *
 * Returns a fresh registry keyed by the same handle IDs the prefix assigned
 * at precompile time, with the new handler installed where provided.
 *
 * @param runImports
 * @param defaults
 * @param shape
 */
export function extractRebindImports(
  runImports: Imports | undefined,
  defaults: HandleRegistry,
  shape: DeclaredImportShape,
): HandleRegistry {
  if (runImports === undefined)
    return defaults

  const merged: HandleRegistry = new Map(defaults)

  for (const [specifier, value] of Object.entries(runImports)) {
    if (typeof value === 'string' || shape.sourceSpecifiers.has(specifier)) {
      throw new UndeclaredImportBindingError(
        `import '${specifier}' is a source module — source imports are frozen `
        + `in the snapshot and cannot be rebound at prefix.run() time`,
      )
    }
    if (typeof value !== 'object' || value === null || Array.isArray(value)) {
      throw new Error(
        `[@iso4/sandbox] imports['${specifier}'] override must be a plain object; `
        + `got ${describeKind(value)}`,
      )
    }
    if (shape.hostFunctionIds[specifier] === undefined) {
      throw new UndeclaredImportBindingError(
        `import '${specifier}' was not declared at precompile time`,
      )
    }
    walkRebind(specifier, value, [], shape, merged)
  }

  return merged
}

function walkRebind(
  specifier: string,
  node: HostModuleObject,
  path: string[],
  shape: DeclaredImportShape,
  merged: HandleRegistry,
): void {
  const fnIds = shape.hostFunctionIds[specifier]!
  const dataPaths = shape.hostDataPaths[specifier]!
  for (const [key, value] of Object.entries(node)) {
    if (value === undefined)
      continue
    const newPath = [...path, key]
    const joined = newPath.join('.')
    if (typeof value === 'function') {
      if (dataPaths.has(joined)) {
        throw new UndeclaredImportBindingError(
          `import '${specifier}'.${joined} is a data leaf, not a function — `
          + `data leaves cannot be rebound`,
        )
      }
      const id = fnIds[joined]
      if (id === undefined) {
        throw new UndeclaredImportBindingError(
          `import '${specifier}'.${joined} was not declared at precompile time`,
        )
      }
      merged.set(id, value as HostExportFunction)
      continue
    }
    if (isPlainObject(value)) {
      walkRebind(specifier, value, newPath, shape, merged)
      continue
    }
    // A non-function on a path that exists (data leaf) or doesn't.
    if (fnIds[joined] !== undefined || dataPaths.has(joined)) {
      throw new UndeclaredImportBindingError(
        `import '${specifier}'.${joined} can only be rebound with a function; `
        + `got ${describeKind(value)}`,
      )
    }
    throw new UndeclaredImportBindingError(
      `import '${specifier}'.${joined} was not declared at precompile time`,
    )
  }
}

// ─────────────────────────────────────────────────────────────────────────
// Source generation for host modules
// ─────────────────────────────────────────────────────────────────────────

/**
 * Generate the ESM source for a host module specifier. Walks `desc`
 * recursively and emits one `export const <name> = <expr>;` per top-level
 * entry. Function leaves become `__iso4_call(<id>, ...args)` stubs; data
 * leaves become JS literals.
 *
 * @param specifier
 * @param desc
 * @param registry
 * @param allocId
 * @param fnIds
 * @param dataPaths
 */
function generateHostModuleSource(
  specifier: string,
  desc: HostModuleObject,
  registry: HandleRegistry,
  allocId: () => number,
  fnIds: Record<string, number>,
  dataPaths: Set<string>,
): string {
  const parts: string[] = []
  for (const [key, value] of Object.entries(desc)) {
    if (value === undefined)
      continue
    if (!isValidExportIdentifier(key)) {
      throw new Error(
        `[@iso4/sandbox] imports['${specifier}'] top-level key '${key}' is not a `
        + `valid JavaScript identifier and cannot be exported as a named ESM export`,
      )
    }
    const expr = emitValue(specifier, value, [key], registry, allocId, fnIds, dataPaths)
    if (key === 'default') {
      parts.push(`export default ${expr};`)
    } else {
      parts.push(`export const ${key} = ${expr};`)
    }
  }
  return parts.join('\n')
}

function emitValue(
  specifier: string,
  value: HostModuleValue,
  path: string[],
  registry: HandleRegistry,
  allocId: () => number,
  fnIds: Record<string, number>,
  dataPaths: Set<string>,
  seen: Set<object> = new Set(),
): string {
  if (typeof value === 'function') {
    const id = allocId()
    registry.set(id, value)
    fnIds[path.join('.')] = id
    // Bridge call is always async (cross-process round trip). The leading
    // arg is the handle ID; `__iso4_call` peels it host-side.
    return `(async (...args) => await globalThis.${BRIDGE_DISPATCH_GLOBAL}(${id}, ...args))`
  }
  if (isPlainObject(value)) {
    if (seen.has(value)) {
      throw new Error(
        `[@iso4/sandbox] imports['${specifier}'].${path.join('.')}: `
        + `circular references are not supported in host-module shapes`,
      )
    }
    seen.add(value)
    const fields: string[] = []
    for (const [key, child] of Object.entries(value)) {
      if (child === undefined)
        continue
      const childExpr = emitValue(
        specifier,
        child,
        [...path, key],
        registry,
        allocId,
        fnIds,
        dataPaths,
        seen,
      )
      fields.push(`${jsObjectKey(key)}: ${childExpr}`)
    }
    seen.delete(value)
    return `({ ${fields.join(', ')} })`
  }
  // Data leaf — emit as a JS literal.
  dataPaths.add(path.join('.'))
  return emitDataLiteral(specifier, value, path)
}

/**
 * Emit a `HostExportData` value as a JS expression that reproduces the
 * original value in the sandbox. Supports primitives, plain objects/arrays,
 * `Date`, `BigInt`, `Uint8Array`. `Map`, `Set`, circular refs, and class
 * instances throw with a clear pointer.
 *
 * @param specifier
 * @param value
 * @param path
 * @param seen
 */
function emitDataLiteral(
  specifier: string,
  value: HostExportData,
  path: string[],
  seen: Set<object> = new Set(),
): string {
  if (value === null)
    return 'null'
  if (value === undefined)
    return 'undefined'
  switch (typeof value) {
    case 'boolean':
      return value ? 'true' : 'false'
    case 'number':
      if (Number.isNaN(value))
        return 'NaN'
      if (!Number.isFinite(value))
        return value > 0 ? 'Infinity' : '-Infinity'
      return String(value)
    case 'bigint':
      return `${value.toString()}n`
    case 'string':
      return JSON.stringify(value)
  }
  if (value instanceof Uint8Array) {
    return `new Uint8Array([${Array.from(value).join(', ')}])`
  }
  if (value instanceof Date) {
    return `new Date(${value.getTime()})`
  }
  if (value instanceof Map || value instanceof Set) {
    throw new Error(
      `[@iso4/sandbox] imports['${specifier}'].${path.join('.')}: `
      + `Map / Set values are not yet supported as data leaves`,
    )
  }
  if (typeof value === 'object') {
    if (seen.has(value)) {
      throw new Error(
        `[@iso4/sandbox] imports['${specifier}'].${path.join('.')}: `
        + `circular references in data leaves are not supported`,
      )
    }
    seen.add(value)
    if (Array.isArray(value)) {
      const parts = value.map((item, i) =>
        emitDataLiteral(specifier, item as HostExportData, [...path, String(i)], seen),
      )
      seen.delete(value)
      return `[${parts.join(', ')}]`
    }
    const proto = Object.getPrototypeOf(value)
    if (proto !== null && proto !== Object.prototype) {
      throw new Error(
        `[@iso4/sandbox] imports['${specifier}'].${path.join('.')}: `
        + `class instances are not supported as data leaves; copy the data `
        + `into a plain object explicitly`,
      )
    }
    const fields: string[] = []
    for (const [key, child] of Object.entries(value as object)) {
      fields.push(`${jsObjectKey(key)}: ${emitDataLiteral(
        specifier,
        child as HostExportData,
        [...path, key],
        seen,
      )}`)
    }
    seen.delete(value)
    return `({ ${fields.join(', ')} })`
  }
  throw new Error(
    `[@iso4/sandbox] imports['${specifier}'].${path.join('.')}: `
    + `unsupported data value of type ${describeKind(value)}`,
  )
}

// ─────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────

function isPlainObject(value: unknown): value is HostModuleDescription {
  if (value === null || typeof value !== 'object')
    return false
  if (Array.isArray(value))
    return false
  if (value instanceof Uint8Array || value instanceof Date)
    return false
  if (value instanceof Map || value instanceof Set)
    return false
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
 * a reserved-word filter. `default` is handled separately by the emitter.
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

/**
 * Emit `key` as a JS object property key: a bare identifier when possible,
 * otherwise a quoted string. Object keys (unlike export names) may be any
 * string, so this never rejects.
 * @param key
 */
function jsObjectKey(key: string): string {
  if (/^[A-Za-z_$][A-Za-z0-9_$]*$/.test(key) && !RESERVED_WORDS.has(key))
    return key
  return JSON.stringify(key)
}

function describeKind(value: unknown): string {
  if (value === null)
    return 'null'
  if (Array.isArray(value))
    return 'array'
  return typeof value
}
