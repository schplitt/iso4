/**
 * Tests for the imports processing layer.
 *
 * Three categories, mirroring `globals.test.ts`:
 * 1. Unit tests — `processImports` / `extractRebindImports`, no binary needed.
 * 2. Type tests — `RebindImports<M>` constraints verified with `@ts-expect-error`.
 * 3. Bad-path / runtime-rejection tests — undeclared specifiers, illegal
 *    rebindings of source modules and data leaves, etc.
 *
 * End-to-end resolver / generated-module behaviour (the sandbox actually
 * importing from these bindings) is covered in `integration.test.ts` and
 * `e2e.test.ts` under "source imports" and "host imports".
 */

import { describe, expect, it } from 'vitest'
import {
  UndeclaredImportBindingError,
  extractRebindImports,
  processImports,
} from '../src/imports.js'
import type { DeclaredImportShape } from '../src/imports.js'
import type { HostExportFunction, Imports, RebindImports } from '../src/types.js'

// ─────────────────────────────────────────────────────────────────────────
// processImports — flattening + shape capture
// ─────────────────────────────────────────────────────────────────────────

describe('processImports', () => {
  it('returns empty bindings + registry + empty shape when imports is undefined', () => {
    const { bindings, registry, shape } = processImports(undefined)
    expect(bindings).toEqual([])
    expect(registry.size).toBe(0)
    expect(shape.sourceSpecifiers.size).toBe(0)
    expect(Object.keys(shape.hostFunctionIds)).toHaveLength(0)
    expect(Object.keys(shape.hostDataPaths)).toHaveLength(0)
  })

  it('passes a string-valued specifier through as a source module', () => {
    const { bindings, registry, shape } = processImports({
      'lib:math': 'export const x = 1',
    })
    expect(bindings).toEqual([{ specifier: 'lib:math', source: 'export const x = 1' }])
    expect(registry.size).toBe(0)
    expect(shape.sourceSpecifiers.has('lib:math')).toBe(true)
  })

  it('lowers an object-valued specifier into generated source + a handle registry', () => {
    const search: HostExportFunction = async (..._args) => 'hit'
    const { bindings, registry, shape } = processImports({
      'host:tools': { search, version: '1.2.3' },
    })
    expect(bindings).toHaveLength(1)
    const binding = bindings[0]!
    expect(binding.specifier).toBe('host:tools')
    // Function leaf → an async stub calling the __iso4_call dispatcher by ID.
    expect(binding.source).toContain('export const search')
    expect(binding.source).toContain('globalThis.__iso4_call(0, ...args)')
    // Data leaf → literal string in the source.
    expect(binding.source).toContain('export const version = "1.2.3"')
    // Handler registered under its handle ID.
    expect(registry.get(0)).toBe(search)
    expect(shape.hostFunctionIds['host:tools']).toEqual({ search: 0 })
    expect(shape.hostDataPaths['host:tools']).toEqual(new Set(['version']))
  })

  it('recursively walks nested mixed data + function objects', () => {
    const greet: HostExportFunction = () => 'hi'
    const { bindings, registry, shape } = processImports({
      'host:nested': {
        someObj: {
          someMethod: greet,
          meta: { name: 'demo' },
        },
      },
    })
    const src = bindings[0]!.source
    // Outer export.
    expect(src).toContain('export const someObj')
    // Inner function gets a dispatcher call keyed by its handle ID.
    expect(src).toContain('globalThis.__iso4_call(0, ...args)')
    // Inner data literal preserved.
    expect(src).toContain('name: "demo"')
    expect(registry.get(0)).toBe(greet)
    expect(shape.hostFunctionIds['host:nested']).toEqual({ 'someObj.someMethod': 0 })
    // `hostDataPaths` records actual data leaves only — intermediate plain
    // objects (like `someObj.meta`) are walked through, not stored.
    expect(shape.hostDataPaths['host:nested']).toEqual(new Set(['someObj.meta.name']))
  })

  it('assigns distinct IDs across multiple modules from one counter', () => {
    const a: HostExportFunction = () => 'a'
    const b: HostExportFunction = () => 'b'
    const { registry, shape } = processImports({
      'host:one': { a },
      'host:two': { b },
    })
    expect(shape.hostFunctionIds['host:one']).toEqual({ a: 0 })
    expect(shape.hostFunctionIds['host:two']).toEqual({ b: 1 })
    expect(registry.get(0)).toBe(a)
    expect(registry.get(1)).toBe(b)
  })

  it('handles default export specially', () => {
    const handler: HostExportFunction = () => 1
    const { bindings, registry, shape } = processImports({
      'host:default': { default: { handler, version: 2 } },
    })
    const src = bindings[0]!.source
    expect(src.startsWith('export default')).toBe(true)
    expect(registry.get(0)).toBe(handler)
    expect(shape.hostFunctionIds['host:default']).toEqual({ 'default.handler': 0 })
  })

  it('emits BigInt and Uint8Array as proper literals', () => {
    const { bindings } = processImports({
      'host:data': {
        big: 9007199254740993n,
        bytes: new Uint8Array([1, 2, 3]),
      },
    })
    const src = bindings[0]!.source
    expect(src).toContain('9007199254740993n')
    expect(src).toContain('new Uint8Array([1, 2, 3])')
  })

  it('rejects Date as a data leaf (wire cannot carry it back out)', () => {
    expect(() =>
      processImports({
        'host:data': { when: new Date(1700000000000) as unknown as never },
      }),
    ).toThrow(/Date values are not supported as data leaves/)
  })
})

// ─────────────────────────────────────────────────────────────────────────
// processImports — rejection paths
// ─────────────────────────────────────────────────────────────────────────

describe('processImports — rejected configurations', () => {
  it('rejects an array as a top-level specifier value', () => {
    expect(() =>
      processImports({ bad: [1, 2, 3] as unknown as never }),
    ).toThrow(/must be a string \(source module\) or a plain object/)
  })

  it('rejects null as a top-level specifier value', () => {
    expect(() =>
      processImports({ bad: null as unknown as never }),
    ).toThrow(/must be a string/)
  })

  it('rejects a Map value in a data leaf', () => {
    expect(() =>
      processImports({
        'host:bad': { config: new Map([['a', 1]]) as unknown as never },
      }),
    ).toThrow(/Map values are not supported as data leaves/)
  })

  it('rejects circular references in data leaves', () => {
    const cyclic: { self?: unknown } = {}
    cyclic.self = cyclic
    expect(() =>
      processImports({
        'host:bad': { cfg: cyclic as unknown as never },
      }),
    ).toThrow(/circular references/)
  })

  it('rejects a top-level key that is not a valid identifier', () => {
    expect(() =>
      processImports({
        'host:bad': { 'not a valid name': 1 },
      }),
    ).toThrow(/not a valid JavaScript identifier/)
  })

  it('rejects class instances (prototype-bearing objects) as data leaves', () => {
    class Cfg {
      mode = 'prod'
    }
    expect(() =>
      processImports({
        'host:bad': { c: new Cfg() as unknown as never },
      }),
    ).toThrow(/class instances are not supported/)
  })
})

// ─────────────────────────────────────────────────────────────────────────
// extractRebindImports — rebinding on prefix.run()
// ─────────────────────────────────────────────────────────────────────────

describe('extractRebindImports', () => {
  const fnA: HostExportFunction = () => 'a'
  const fnB: HostExportFunction = () => 'b'

  function makeFixture() {
    const { registry, shape } = processImports({
      'host:tools': { search: fnA, version: '1.0.0' },
      'lib:math': 'export const x = 1',
    })
    return { registry, shape }
  }

  it('returns the precompile defaults when no override is passed', () => {
    const { registry, shape } = makeFixture()
    expect(extractRebindImports(undefined, registry, shape)).toBe(registry)
  })

  it('overrides a declared function leaf', () => {
    const { registry, shape } = makeFixture()
    const out = extractRebindImports(
      { 'host:tools': { search: fnB } },
      registry,
      shape,
    )
    // `search` was handle 0.
    expect(out.get(0)).toBe(fnB)
  })

  it('overrides a declared function leaf at a nested path', () => {
    const greet: HostExportFunction = () => 'hi'
    const replaced: HostExportFunction = () => 'bye'
    const { registry, shape } = processImports({
      'host:nested': { someObj: { someMethod: greet, meta: 'x' } },
    })
    const out = extractRebindImports(
      { 'host:nested': { someObj: { someMethod: replaced } } },
      registry,
      shape,
    )
    expect(out.get(0)).toBe(replaced)
  })

  it('omitted overrides fall back to the precompile-time handler', () => {
    const fnX: HostExportFunction = () => 'x'
    const fnY: HostExportFunction = () => 'y'
    const { registry, shape } = processImports({
      'host:multi': { a: fnX, b: fnY },
    })
    const out = extractRebindImports(
      { 'host:multi': { a: () => 'new-a' } },
      registry,
      shape,
    )
    // a=0, b=1.
    expect(out.get(0)?.()).toBe('new-a')
    expect(out.get(1)).toBe(fnY)
  })
})

// ─────────────────────────────────────────────────────────────────────────
// extractRebindImports — undeclared bindings rejection
// ─────────────────────────────────────────────────────────────────────────

describe('extractRebindImports — UndeclaredImportBindingError', () => {
  const empty: DeclaredImportShape = {
    sourceSpecifiers: new Set(),
    hostFunctionIds: {},
    hostDataPaths: {},
  }

  it('refuses to rebind an undeclared specifier', () => {
    expect(() =>
      extractRebindImports({ 'host:never-declared': { fn: () => 1 } }, new Map(), empty),
    ).toThrow(UndeclaredImportBindingError)
  })

  it('UndeclaredImportBindingError carries the ERR_UNDECLARED_BINDING code', () => {
    try {
      extractRebindImports({ 'host:x': { fn: () => 1 } }, new Map(), empty)
      throw new Error('should have thrown')
    } catch (e) {
      expect(e).toBeInstanceOf(UndeclaredImportBindingError)
      expect((e as UndeclaredImportBindingError).code).toBe('ERR_UNDECLARED_BINDING')
    }
  })

  it('refuses to rebind a source-module specifier with a string', () => {
    const { registry, shape } = processImports({
      'lib:math': 'export const x = 1',
    })
    expect(() =>
      extractRebindImports(
        { 'lib:math': 'export const x = 2' },
        registry,
        shape,
      ),
    ).toThrow(/source imports are frozen/)
  })

  it('refuses to rebind a source-module specifier with an object', () => {
    const { registry, shape } = processImports({
      'lib:math': 'export const x = 1',
    })
    expect(() =>
      extractRebindImports(
        { 'lib:math': { whatever: () => 1 } },
        registry,
        shape,
      ),
    ).toThrow(/source imports are frozen/)
  })

  it('refuses to rebind a data leaf of a declared host module', () => {
    const { registry, shape } = processImports({
      'host:cfg': { version: '1.0.0' },
    })
    expect(() =>
      extractRebindImports(
        { 'host:cfg': { version: () => '2.0.0' } },
        registry,
        shape,
      ),
    ).toThrow(/'host:cfg'.version is a data leaf, not a function/)
  })

  it('refuses to rebind an undeclared function name on a declared specifier', () => {
    const { registry, shape } = processImports({
      'host:tools': { search: () => 1 },
    })
    expect(() =>
      extractRebindImports(
        { 'host:tools': { undeclared: () => 1 } },
        registry,
        shape,
      ),
    ).toThrow(/'host:tools'.undeclared was not declared/)
  })

  it('refuses a data value where a function was declared', () => {
    const { registry, shape } = processImports({
      'host:tools': { search: () => 1 },
    })
    expect(() =>
      extractRebindImports(
        { 'host:tools': { search: 'not-a-function' as unknown as never } },
        registry,
        shape,
      ),
    ).toThrow(/can only be rebound with a function/)
  })
})

// ─────────────────────────────────────────────────────────────────────────
// Type-level constraints — RebindImports
// ─────────────────────────────────────────────────────────────────────────

describe('RebindImports type constraints (compile-time)', () => {
  type SearchFn = (q: string) => Promise<string[]>

  type M = Imports<{
    'host:tools': {
      search: SearchFn
      version: string
    }
    'lib:math': string
  }>

  type Rebind = RebindImports<M>

  it('declared host function exports are rebindable with the same signature', () => {
    const valid: Rebind = {
      'host:tools': { search: async (_q: string) => ['hit'] },
    }
    expect(valid).toBeDefined()
  })

  it('source-module specifiers are excluded from the rebind keyspace', () => {
    // @ts-expect-error — 'lib:math' is a source module and must not appear
    //                    as a rebindable key in RebindImports<M>.
    const _invalid: Rebind = { 'lib:math': 'export const x = 2' }
    expect(true).toBe(true)
  })

  it('undeclared specifier is a TypeScript error', () => {
    // @ts-expect-error — undeclared specifiers are excluded from RebindImports<M>.
    const _invalid: Rebind = {
      'host:never-declared': { fn: () => 1 },
    }
    expect(true).toBe(true)
  })

  it('data leaves are excluded from the rebindable export shape', () => {
    // @ts-expect-error — `version` is a data leaf (string), not a function.
    const _invalid: Rebind = {
      'host:tools': { version: '2.0.0' as const },
    }
    expect(true).toBe(true)
  })

  it('declared function rebinding must match the declared signature', () => {
    // @ts-expect-error — `search` takes (string) and returns Promise<string[]>;
    //                    a (number) => number is not assignable.
    const _invalid: Rebind = {
      'host:tools': { search: (_n: number) => _n + 1 },
    }
    expect(true).toBe(true)
  })

  it('nested function leaves are reachable through the rebind walker', () => {
    type N = Imports<{
      'host:nested': { someObj: { someMethod: () => Promise<number>, meta: string } }
    }>
    type RN = RebindImports<N>
    const valid: RN = {
      'host:nested': { someObj: { someMethod: async () => 42 } },
    }
    expect(valid).toBeDefined()

    // @ts-expect-error — `meta` is a data leaf, not rebindable.
    const _invalid: RN = {
      'host:nested': { someObj: { meta: 'new' } },
    }
    expect(true).toBe(true)
  })

  it('omitting the whole imports field is allowed (defaults reused)', () => {
    const empty: Rebind = {}
    expect(empty).toBeDefined()
  })
})
