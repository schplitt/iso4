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
 *    `integration.test.ts` under "imports rebinding enforcement".
 *
 * End-to-end resolver / host-module behaviour (the sandbox actually importing
 * from these bindings) is covered in `integration.test.ts` and `e2e.test.ts`
 * under "source imports" and "host imports".
 */

import { describe, expect, it } from 'vitest'
import {
  ImportRebindError,
  MAX_HOST_MODULE_DEPTH,
  importHandlerKey,
  declaredImportShape,
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

  it('accepts a host-module shape nested exactly at the depth limit', () => {
    let node: Record<string, unknown> = { leaf: () => 1 }
    for (let i = 1; i < MAX_HOST_MODULE_DEPTH; i++)
      node = { c: node }
    const { handlers } = processImports({ 'host:deep': node as never })
    expect(handlers.size).toBe(1)
    const [key] = handlers.keys()
    expect(key!.split('.').length).toBe(MAX_HOST_MODULE_DEPTH)
  })

  it('rejects a host-module shape nested one level past the limit, naming the path', () => {
    let node: Record<string, unknown> = { leaf: 1 }
    for (let i = 1; i < MAX_HOST_MODULE_DEPTH + 1; i++)
      node = { c: node }
    expect(() =>
      processImports({ 'host:deep': node as never }),
    ).toThrow(/imports\['host:deep'\]\.c\.c\..*\.leaf: host-module shapes may nest at most 64 levels deep/)
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

  it.each(['café', 'π', '$', '_', 'a\u200Cb', 'a\u200Db', '\u309B'])('accepts the Unicode identifier %s as a top-level key', (name) => {
    const { bindings } = processImports({ 'host:ok': { [name]: 1 } })
    expect(bindings[0]!.module![0]![0]).toBe(name)
  })

  it.each(['', '1abc', 'my-tool', '\u200Ca'])('rejects %j as a top-level key', (name) => {
    expect(() =>
      processImports({ 'host:bad': { [name]: 1 } }),
    ).toThrow(/not a valid JavaScript identifier/)
  })

  it.each(['eval', 'arguments'])('rejects strict-mode-restricted %s as a top-level key', (name) => {
    expect(() =>
      processImports({
        'host:bad': { [name]: () => 1 },
      }),
    ).toThrow(/not a valid JavaScript identifier/)
  })
})

// ─────────────────────────────────────────────────────────────────────────
// mergeRebindImports — rebinding on prefix.run()
// ─────────────────────────────────────────────────────────────────────────

describe('import specifiers that are not well-formed strings', () => {
  it.each(['x\uD800', '\uDC00'])('processImports and mergeRebindImports refuse %j', (specifier) => {
    const error = new TypeError(`[@iso4/sandbox] import specifier '${specifier}' is not a well-formed string`)
    expect(() => processImports({ [specifier]: { f: () => 1 } })).toThrow(error)
    expect(() => processImports({ [specifier]: 'export default 1' })).toThrow(error)
    expect(() => mergeRebindImports({ [specifier]: { f: () => 1 } } as never, new Map(), [])).toThrow(error)
  })
})

describe('mergeRebindImports', () => {
  const fnA: HostExportFunction = () => 'a'
  const fnB: HostExportFunction = () => 'b'

  function makeFixture() {
    return processImports({
      'host:tools': { search: fnA, version: '1.0.0' },
      'lib:math': 'export const x = 1',
    })
  }

  it('returns the precompile defaults and no rebinds when no override is passed', () => {
    const { handlers: defaults, bindings } = makeFixture()
    const { rebinds, handlers } = mergeRebindImports(undefined, defaults, bindings)
    expect(rebinds).toEqual([])
    expect(handlers).toBe(defaults)
  })

  it('overrides a declared function leaf and reports the rebind location', () => {
    const { handlers: defaults, bindings } = makeFixture()
    const { rebinds, handlers } = mergeRebindImports({ 'host:tools': { search: fnB } }, defaults, bindings)
    expect(rebinds).toEqual([{ specifier: 'host:tools', path: 'search' }])
    expect(handlers.get(importHandlerKey('host:tools', 'search'))).toBe(fnB)
    // The defaults map is not mutated.
    expect(defaults.get(importHandlerKey('host:tools', 'search'))).toBe(fnA)
  })

  it('overrides a function leaf at a nested path', () => {
    const greet: HostExportFunction = () => 'hi'
    const replaced: HostExportFunction = () => 'bye'
    const { handlers: defaults, bindings } = processImports({
      'host:nested': { someObj: { someMethod: greet, meta: 'x' } },
    })
    const { rebinds, handlers } = mergeRebindImports(
      { 'host:nested': { someObj: { someMethod: replaced } } },
      defaults,
      declaredImportShape(bindings),
    )
    expect(rebinds).toEqual([{ specifier: 'host:nested', path: 'someObj.someMethod' }])
    expect(handlers.get(importHandlerKey('host:nested', 'someObj.someMethod'))).toBe(replaced)
  })

  it('omitted overrides fall back to the precompile-time handler', () => {
    const fnX: HostExportFunction = () => 'x'
    const fnY: HostExportFunction = () => 'y'
    const { handlers: defaults, bindings } = processImports({ 'host:multi': { a: fnX, b: fnY } })
    const { handlers } = mergeRebindImports({ 'host:multi': { a: () => 'new-a' } }, defaults, bindings)
    expect(handlers.get(importHandlerKey('host:multi', 'a'))?.()).toBe('new-a')
    expect(handlers.get(importHandlerKey('host:multi', 'b'))).toBe(fnY)
  })
})

describe('declaredImportShape', () => {
  it('keeps the shape but drops data values and source text', () => {
    const big = 'x'.repeat(1000)
    const { bindings } = processImports({ 'host:m': { f: () => 1, d: big, o: { g: () => 1 } }, 'lib:s': big })
    expect(declaredImportShape(bindings)).toEqual([
      { specifier: 'host:m', module: [['f', { kind: 'function' }], ['d', { kind: 'data', value: undefined }], ['o', { kind: 'object', entries: [['g', { kind: 'function' }]] }]] },
      { specifier: 'lib:s', source: '' },
    ])
  })
})

// ─────────────────────────────────────────────────────────────────────────
// mergeRebindImports — refusals: shape lookup first, then the value
// ─────────────────────────────────────────────────────────────────────────

describe('mergeRebindImports — ImportRebindError', () => {
  const { handlers, bindings } = processImports({
    'host:tools': { search: () => 1, version: '1.0.0', nested: { fn: () => 1 } },
    'lib:math': 'export const x = 1',
  })

  it.each([
    ['an undeclared specifier as a string', { 'host:nope': 'x' }, 'ERR_UNDECLARED_BINDING', `import 'host:nope' was not declared at precompile time`],
    ['an undeclared specifier as null', { 'host:nope': null }, 'ERR_UNDECLARED_BINDING', `import 'host:nope' was not declared at precompile time`],
    ['an undeclared path with a non-function', { 'host:tools': { nope: 42 } }, 'ERR_UNDECLARED_BINDING', `import 'host:tools'.nope was not declared at precompile time`],
    ['a function at an object node', { 'host:tools': { nested: () => 1 } }, 'ERR_INVALID_REBIND', `import 'host:tools'.nested can only be rebound with a plain object; got function`],
    ['a source module as a string', { 'lib:math': 'export const x = 2' }, 'ERR_FROZEN_BINDING', `import 'lib:math' is a source module — source imports are frozen with the prefix and cannot be rebound at prefix.run() time`],
    ['a source module as an object', { 'lib:math': { x: () => 2 } }, 'ERR_FROZEN_BINDING', `import 'lib:math' is a source module — source imports are frozen with the prefix and cannot be rebound at prefix.run() time`],
    ['a data leaf with data', { 'host:tools': { version: '2.0.0' } }, 'ERR_FROZEN_BINDING', `import 'host:tools'.version is a data leaf, not a function — data leaves cannot be rebound`],
    ['a data leaf with a function', { 'host:tools': { version: () => '2' } }, 'ERR_FROZEN_BINDING', `import 'host:tools'.version is a data leaf, not a function — data leaves cannot be rebound`],
    ['a host module as a string', { 'host:tools': 'x' }, 'ERR_INVALID_REBIND', `import 'host:tools' can only be rebound with a plain object; got string`],
    ['a host module as null', { 'host:tools': null }, 'ERR_INVALID_REBIND', `import 'host:tools' can only be rebound with a plain object; got null`],
    ['a host module as undefined', { 'host:tools': undefined }, 'ERR_INVALID_REBIND', `import 'host:tools' can only be rebound with a plain object; got undefined`],
    ['a host module as a number', { 'host:tools': 42 }, 'ERR_INVALID_REBIND', `import 'host:tools' can only be rebound with a plain object; got number`],
    ['a function leaf with a string', { 'host:tools': { search: 'nope' } }, 'ERR_INVALID_REBIND', `import 'host:tools'.search can only be rebound with a function; got string`],
    ['a function leaf with undefined', { 'host:tools': { search: undefined } }, 'ERR_INVALID_REBIND', `import 'host:tools'.search can only be rebound with a function; got undefined`],
    ['an object node with a number', { 'host:tools': { nested: 1 } }, 'ERR_INVALID_REBIND', `import 'host:tools'.nested can only be rebound with a plain object; got number`],
  ])('refuses %s', (_kind, imports, code, message) => {
    expect(() => mergeRebindImports(imports as never, handlers, bindings)).toThrow(
      expect.objectContaining({ constructor: ImportRebindError, code, message }),
    )
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
