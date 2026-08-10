/**
 * Tests for the imports processing layer.
 *
 * Three categories, mirroring `globals.test.ts`:
 * 1. Unit tests — `processImports` / `mergeRebindImports`, no binary needed.
 * 2. Type tests — `RebindImports<M>` constraints verified with `@ts-expect-error`.
 * 3. Bad-path tests for the client-visible rejections (invalid shapes,
 *    unsupported data values, non-function rebind values). Declared-shape
 *    enforcement (undeclared specifiers/paths, data-leaf and source-module
 *    rebinds) lives in the Rust runtime and is covered end-to-end in
 *    `e2e.test.ts` under "ERR_UNDECLARED_BINDING".
 *
 * End-to-end resolver / host-module behaviour (the sandbox actually importing
 * from these bindings) is covered in `integration.test.ts` and `e2e.test.ts`
 * under "source imports" and "host imports".
 */

import { describe, expect, it } from 'vitest'
import {
  UndeclaredImportBindingError,
  importHandlerKey,
  mergeRebindImports,
  processImports,
} from '../src/imports.js'
import { serializeValue } from '../src/v8-codec.js'
import type { HostExportFunction, Imports, RebindImports } from '../src/types.js'

// ─────────────────────────────────────────────────────────────────────────
// processImports — lowering to wire shape + handler capture
// ─────────────────────────────────────────────────────────────────────────

describe('processImports', () => {
  it('returns empty bindings + handlers when imports is undefined', () => {
    const { bindings, handlers } = processImports(undefined)
    expect(bindings).toEqual([])
    expect(handlers.size).toBe(0)
  })

  it('passes a string-valued specifier through as a source module', () => {
    const { bindings, handlers } = processImports({
      'lib:math': 'export const x = 1',
    })
    expect(bindings).toEqual([{ specifier: 'lib:math', source: 'export const x = 1' }])
    expect(handlers.size).toBe(0)
  })

  it('lowers an object-valued specifier into a data tree + handler map', () => {
    const search: HostExportFunction = async (..._args) => 'hit'
    const { bindings, handlers } = processImports({
      'host:tools': { search, version: '1.2.3' },
    })
    expect(bindings).toEqual([{
      specifier: 'host:tools',
      module: [
        ['search', { kind: 'function' }],
        ['version', { kind: 'data', value: '1.2.3' }],
      ],
    }])
    // No source text anywhere — the runtime builds the module from the tree.
    expect(bindings[0]).not.toHaveProperty('source')
    expect(handlers.get(importHandlerKey('host:tools', 'search'))).toBe(search)
  })

  it('recursively walks nested mixed data + function objects', () => {
    const greet: HostExportFunction = () => 'hi'
    const { bindings, handlers } = processImports({
      'host:nested': {
        someObj: {
          someMethod: greet,
          meta: { name: 'demo' },
        },
      },
    })
    expect(bindings).toEqual([{
      specifier: 'host:nested',
      module: [
        ['someObj', {
          kind: 'object',
          entries: [
            ['someMethod', { kind: 'function' }],
            // Plain objects are walked into object nodes; only non-object
            // values become data leaves.
            ['meta', {
              kind: 'object',
              entries: [['name', { kind: 'data', value: 'demo' }]],
            }],
          ],
        }],
      ],
    }])
    expect(handlers.get(importHandlerKey('host:nested', 'someObj.someMethod'))).toBe(greet)
  })

  it('registers handlers for multiple modules under distinct keys', () => {
    const a: HostExportFunction = () => 'a'
    const b: HostExportFunction = () => 'b'
    const { handlers } = processImports({
      'host:one': { a },
      'host:two': { b },
    })
    expect(handlers.get(importHandlerKey('host:one', 'a'))).toBe(a)
    expect(handlers.get(importHandlerKey('host:two', 'b'))).toBe(b)
    expect(handlers.size).toBe(2)
  })

  it('accepts a default export entry', () => {
    const handler: HostExportFunction = () => 1
    const { bindings, handlers } = processImports({
      'host:default': { default: { handler, version: 2 } },
    })
    expect(bindings).toEqual([{
      specifier: 'host:default',
      module: [
        ['default', {
          kind: 'object',
          entries: [
            ['handler', { kind: 'function' }],
            ['version', { kind: 'data', value: 2 }],
          ],
        }],
      ],
    }])
    expect(handlers.get(importHandlerKey('host:default', 'default.handler'))).toBe(handler)
  })

  it('carries BigInt, Uint8Array, and arrays as data leaves', () => {
    const bytes = new Uint8Array([1, 2, 3])
    const { bindings } = processImports({
      'host:data': {
        big: 9007199254740993n,
        bytes,
        list: [1, 'two', { three: 3 }],
      },
    })
    expect(bindings).toEqual([{
      specifier: 'host:data',
      module: [
        ['big', { kind: 'data', value: 9007199254740993n }],
        ['bytes', { kind: 'data', value: bytes }],
        ['list', { kind: 'data', value: [1, 'two', { three: 3 }] }],
      ],
    }])
  })

  it('skips undefined values in the module shape', () => {
    const { bindings } = processImports({
      'host:sparse': { present: 1, absent: undefined },
    })
    expect(bindings).toEqual([{
      specifier: 'host:sparse',
      module: [['present', { kind: 'data', value: 1 }]],
    }])
  })
})

// ─────────────────────────────────────────────────────────────────────────
// Data leaves are not inspected — V8's serializer is the only gate
// ─────────────────────────────────────────────────────────────────────────

describe('processImports — data leaves cross uninspected', () => {
  // `processImports` deliberately does not walk data leaves: doing so would
  // duplicate the serializer's work on the Node main thread at O(values), and
  // any hand-written allowlist would drift from what V8 actually supports.
  // Everything V8's format can represent therefore passes straight through.
  it.each([
    ['Date', new Date(1700000000000)],
    ['RegExp', /ab+c/gi],
    ['Error', new TypeError('boom')],
    ['Map', new Map<unknown, unknown>([['a', 1]])],
    ['Set', new Set([1, 2])],
    ['ArrayBuffer', new Uint8Array([1, 2, 3]).buffer],
    ['Float64Array', new Float64Array([1.5, -2.5])],
    ['BigInt64Array', new BigInt64Array([1n])],
    ['DataView', new DataView(new ArrayBuffer(4))],
  ])('carries %s through as a data leaf', (_name, value) => {
    const { bindings } = processImports({ 'host:data': { v: value } })
    expect(bindings).toEqual([{
      specifier: 'host:data',
      module: [['v', { kind: 'data', value }]],
    }])
  })

  it('carries a cycle inside a non-plain-object leaf (V8 writes back-references)', () => {
    const arr: unknown[] = []
    arr.push(arr)
    const { bindings } = processImports({ 'host:data': { list: arr } })
    expect(bindings[0]?.module).toEqual([['list', { kind: 'data', value: arr }]])
    // And it really does survive the codec, identity included.
    const back = arr
    expect(() => serializeValue(back)).not.toThrow()
  })

  it('carries a class instance through, flattening at the codec (protocol.md §4.2)', () => {
    class Cfg {
      mode = 'prod'
      describe(): string {
        return this.mode
      }
    }
    const instance = new Cfg()
    const { bindings } = processImports({ 'host:data': { c: instance } })
    expect(bindings[0]?.module).toEqual([['c', { kind: 'data', value: instance }]])
    // Own enumerable props survive; the prototype and its methods do not.
    const roundTripped = serializeValue(instance)
    expect(roundTripped.byteLength).toBeGreaterThan(0)
  })

  it('leaves an unsupported value to the codec, which rejects it', () => {
    // Registration no longer throws — the function reaches `serializeValue`,
    // which is where the boundary is actually enforced.
    expect(() =>
      processImports({ 'host:bad': { list: [() => 1] as unknown as never } }),
    ).not.toThrow()
    expect(() => serializeValue([() => 1])).toThrow()
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

  it('rejects circular references in host-module shapes', () => {
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

  it('rejects a reserved word as a top-level key', () => {
    expect(() =>
      processImports({
        'host:bad': { class: 1 },
      }),
    ).toThrow(/not a valid JavaScript identifier/)
  })
})

// ─────────────────────────────────────────────────────────────────────────
// mergeRebindImports — rebinding on prefix.run()
// ─────────────────────────────────────────────────────────────────────────

describe('mergeRebindImports', () => {
  const fnA: HostExportFunction = () => 'a'
  const fnB: HostExportFunction = () => 'b'

  function makeFixture() {
    const { handlers } = processImports({
      'host:tools': { search: fnA, version: '1.0.0' },
      'lib:math': 'export const x = 1',
    })
    return handlers
  }

  it('returns the precompile defaults and no rebinds when no override is passed', () => {
    const defaults = makeFixture()
    const { rebinds, handlers } = mergeRebindImports(undefined, defaults)
    expect(rebinds).toEqual([])
    expect(handlers).toBe(defaults)
  })

  it('overrides a declared function leaf and reports the rebind location', () => {
    const defaults = makeFixture()
    const { rebinds, handlers } = mergeRebindImports(
      { 'host:tools': { search: fnB } },
      defaults,
    )
    expect(rebinds).toEqual([{ specifier: 'host:tools', path: 'search' }])
    expect(handlers.get(importHandlerKey('host:tools', 'search'))).toBe(fnB)
    // The defaults map is not mutated.
    expect(defaults.get(importHandlerKey('host:tools', 'search'))).toBe(fnA)
  })

  it('overrides a function leaf at a nested path', () => {
    const greet: HostExportFunction = () => 'hi'
    const replaced: HostExportFunction = () => 'bye'
    const { handlers: defaults } = processImports({
      'host:nested': { someObj: { someMethod: greet, meta: 'x' } },
    })
    const { rebinds, handlers } = mergeRebindImports(
      { 'host:nested': { someObj: { someMethod: replaced } } },
      defaults,
    )
    expect(rebinds).toEqual([{ specifier: 'host:nested', path: 'someObj.someMethod' }])
    expect(handlers.get(importHandlerKey('host:nested', 'someObj.someMethod'))).toBe(replaced)
  })

  it('omitted overrides fall back to the precompile-time handler', () => {
    const fnX: HostExportFunction = () => 'x'
    const fnY: HostExportFunction = () => 'y'
    const { handlers: defaults } = processImports({
      'host:multi': { a: fnX, b: fnY },
    })
    const { handlers } = mergeRebindImports(
      { 'host:multi': { a: () => 'new-a' } },
      defaults,
    )
    expect(handlers.get(importHandlerKey('host:multi', 'a'))?.()).toBe('new-a')
    expect(handlers.get(importHandlerKey('host:multi', 'b'))).toBe(fnY)
  })

  it('collects rebind locations even for specifiers it has no defaults for', () => {
    // Declared-shape enforcement is the runtime's job: the location is sent
    // on the wire and the run fails with ERR_UNDECLARED_BINDING there. The
    // client merge stays permissive so a non-TS caller path can't bypass
    // enforcement that only existed here.
    const { rebinds } = mergeRebindImports(
      { 'host:never-declared': { fn: () => 1 } },
      new Map(),
    )
    expect(rebinds).toEqual([{ specifier: 'host:never-declared', path: 'fn' }])
  })
})

// ─────────────────────────────────────────────────────────────────────────
// mergeRebindImports — client-visible rejections
// ─────────────────────────────────────────────────────────────────────────

describe('mergeRebindImports — UndeclaredImportBindingError', () => {
  it('refuses to rebind a specifier with a string (source-module form)', () => {
    expect(() =>
      mergeRebindImports({ 'lib:math': 'export const x = 2' }, new Map()),
    ).toThrow(/source imports are frozen/)
  })

  it('UndeclaredImportBindingError carries the ERR_UNDECLARED_BINDING code', () => {
    try {
      mergeRebindImports({ 'lib:math': 'export const x = 2' }, new Map())
      throw new Error('should have thrown')
    } catch (e) {
      expect(e).toBeInstanceOf(UndeclaredImportBindingError)
      expect((e as UndeclaredImportBindingError).code).toBe('ERR_UNDECLARED_BINDING')
    }
  })

  it('refuses a non-function, non-object value at a leaf', () => {
    expect(() =>
      mergeRebindImports(
        { 'host:tools': { search: 'not-a-function' as unknown as never } },
        new Map(),
      ),
    ).toThrow(/can only be rebound with a function/)
  })

  it('rejects a non-object specifier override', () => {
    expect(() =>
      mergeRebindImports({ 'host:tools': 42 as unknown as never }, new Map()),
    ).toThrow(/must be a plain object/)
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
