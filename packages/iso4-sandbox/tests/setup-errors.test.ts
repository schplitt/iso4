/**
 * Setup errors the client detects before anything reaches the runtime, and the
 * runtime fallback that throws the same error when only the runtime can tell.
 */

import type { MockInstance } from 'vitest'
import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, test, vi } from 'vitest'
import { RuntimeIpcClient } from '../src/client.js'
import { createSandbox } from '../src/index.js'
import { RunPool } from '../src/pool.js'
import type { Prefix, Sandbox } from '../src/types.js'

let sandbox: Sandbox
let prefix: Prefix<any, any>
let wire: MockInstance[]

beforeAll(async () => {
  sandbox = await createSandbox()
  prefix = await sandbox.prepare({
    id: crypto.randomUUID(),
    code: `
      import { add, nested, dotted } from 'host:tools'
      export default () => 1
      export const rebinds = async () => [await tool(), await shimmed(), await add(1), await nested.fn()]
      export const dottedPair = async () => [await dotted['a.b'](), await dotted.a.b()]
    `,
    globals: {
      tool: async () => 1,
      shimmed: { kind: 'bridge-with-shim', shim: '(h) => h', handler: async () => 1 },
      config: { kind: 'data', value: 1 },
      expr: '1',
    },
    imports: {
      'host:tools': {
        add: (n: number) => n + 1,
        version: '1.0.0',
        nested: { fn: () => 1 },
        dotted: { 'a.b': () => 'dotted', 'a': { b: () => 'nested' }, 'v.x': '1' },
        flat: { 'c.d': () => 1 },
        deep: { c: { d: () => 1 } },
      },
      'lib:math': 'export const x = 1',
    },
  })
})

afterAll(async () => {
  await prefix?.dispose()
  await sandbox?.dispose()
})

beforeEach(() => {
  wire = [
    vi.spyOn(RunPool.prototype, 'withClient'),
    vi.spyOn(RuntimeIpcClient.prototype, 'runRawCode'),
    vi.spyOn(RuntimeIpcClient.prototype, 'prefixRun'),
    vi.spyOn(RuntimeIpcClient.prototype, 'precompile'),
  ]
})

afterEach(() => {
  vi.restoreAllMocks()
})

/**
 * Await `promise`, expect a plain coded `Error`, and expect that nothing took
 * a slot or reached the runtime.
 * @param promise the refused call
 * @param code the expected setup code
 * @param message the expected message, the same one the runtime sends
 */
async function expectClientRefusal(promise: Promise<unknown>, code: string, message?: RegExp | string): Promise<void> {
  const error = await promise.then(() => undefined, (e: unknown) => e)
  expect(error).toBeInstanceOf(Error)
  expect(Object.getPrototypeOf(error)).toBe(Error.prototype)
  expect(error).toMatchObject({ name: 'Error', code })
  if (message !== undefined)
    expect((error as Error).message).toMatch(message)
  for (const spy of wire)
    expect(spy).not.toHaveBeenCalled()
}

const call = (options: object): Promise<unknown> => prefix.call({ export: 'default', ...options } as never)

describe('ERR_RESERVED_NAME', () => {
  test.each(['console', 'Response', 'NaN', '__proto__'])('run() and prepare() refuse %s without a round trip', async (name) => {
    const message = `global '${name}' is reserved by the runtime and cannot be provided by the host`
    await expectClientRefusal(sandbox.run({ code: 'export default 1', globals: { [name]: { kind: 'data', value: 1 } } as never }), 'ERR_RESERVED_NAME', message)
    await expectClientRefusal(sandbox.prepare({ id: crypto.randomUUID(), code: '', globals: { [name]: async () => 1 } as never }), 'ERR_RESERVED_NAME', message)
  })
})

describe('ERR_UNDECLARED_BINDING', () => {
  test.each([
    ['a function global', { globals: { zzz: async () => 1 } }, `global 'zzz' was not declared at precompile time`],
    ['a data global', { globals: { zzz: { kind: 'data', value: 1 } } }, `global 'zzz' was not declared at precompile time`],
    ['a string global', { globals: { zzz: '1' } }, `global 'zzz' was not declared at precompile time`],
    ['a runtime-owned name', { globals: { console: async () => 1 } }, `global 'console' was not declared at precompile time`],
    ['an Object.prototype key', { globals: Object.defineProperty({}, 'toString', { value: async () => 1, enumerable: true }) }, /toString/],
    ['an import specifier', { imports: { 'host:other': { fn: () => 1 } } }, `import 'host:other' was not declared at precompile time`],
    ['an import path', { imports: { 'host:tools': { nope: () => 1 } } }, `import 'host:tools'.nope was not declared at precompile time`],
    ['an undeclared specifier as a string', { imports: { 'host:nope': 'x' } }, `import 'host:nope' was not declared at precompile time`],
    ['an undeclared specifier as null', { imports: { 'host:nope': null } }, `import 'host:nope' was not declared at precompile time`],
    ['an undeclared path with a non-function', { imports: { 'host:tools': { nope: 42 } } }, `import 'host:tools'.nope was not declared at precompile time`],
  ])('prefix.call() refuses %s without a round trip', async (_kind, options, message) => {
    await expectClientRefusal(call(options), 'ERR_UNDECLARED_BINDING', message)
  })
})

describe('ERR_FROZEN_BINDING', () => {
  test.each([
    ['a data global rebound with a function', { globals: { config: async () => 1 } }, /global 'config' is a value global, frozen with the prefix/],
    ['a data global rebound with data', { globals: { config: { kind: 'data', value: 2 } } }, /global 'config' is a value global/],
    ['a string global', { globals: { expr: async () => 1 } }, /global 'expr' is a value global/],
    ['a source module', { imports: { 'lib:math': 'export const x = 2' } }, /source imports are frozen/],
    ['a source module rebound as a host module', { imports: { 'lib:math': { x: () => 2 } } }, /source imports are frozen/],
    ['a data leaf rebound with a function', { imports: { 'host:tools': { version: () => '2' } } }, /is a data leaf, not a function/],
    ['a data leaf rebound with data', { imports: { 'host:tools': { version: '2.0.0' } } }, /is a data leaf, not a function/],
  ])('prefix.call() refuses %s without a round trip', async (_kind, options, message) => {
    await expectClientRefusal(call(options), 'ERR_FROZEN_BINDING', message)
  })
})

describe('ERR_INVALID_REBIND', () => {
  test.each([
    ['a bridge global rebound with a string', { globals: { tool: 'https://evil.test' } }, /global 'tool' can only be rebound with a function; got string/],
    ['a bridge global rebound with undefined', { globals: { tool: undefined } }, /global 'tool' can only be rebound with a function; got undefined/],
    ['a shim global rebound with data', { globals: { shimmed: { kind: 'data', value: 1 } } }, /global 'shimmed' can only be rebound with a function/],
    ['a host module rebound with a string', { imports: { 'host:tools': 'x' } }, `import 'host:tools' can only be rebound with a plain object; got string`],
    ['a host module rebound with null', { imports: { 'host:tools': null } }, `import 'host:tools' can only be rebound with a plain object; got null`],
    ['a host module rebound with undefined', { imports: { 'host:tools': undefined } }, `import 'host:tools' can only be rebound with a plain object; got undefined`],
    ['a function leaf rebound with undefined', { imports: { 'host:tools': { add: undefined } } }, `import 'host:tools'.add can only be rebound with a function; got undefined`],
    ['an object node rebound with a function', { imports: { 'host:tools': { nested: () => 1 } } }, `import 'host:tools'.nested can only be rebound with a plain object; got function`],
    ['a function leaf rebound with an object', { imports: { 'host:tools': { add: { deeper: () => 1 } } } }, `import 'host:tools'.add can only be rebound with a function; got object`],
  ])('prefix.call() refuses %s without a round trip', async (_kind, options, message) => {
    await expectClientRefusal(call(options), 'ERR_INVALID_REBIND', message)
  })
})

describe('malformed options', () => {
  test('prepare() throws a TypeError when code is not a string, without a round trip', async () => {
    for (const code of [undefined, null, 5, ['export default 1']]) {
      await expect(sandbox.prepare({ id: crypto.randomUUID(), code: code as never })).rejects.toThrow(TypeError)
    }
    for (const spy of wire)
      expect(spy).not.toHaveBeenCalled()
  })

  test.each([-1, 1.5])('prefix.call() throws a TypeError for a bad limit (%s) without a round trip', async (cpuTimeMs) => {
    await expect(call({ limits: { cpuTimeMs } })).rejects.toThrow(TypeError)
    for (const spy of wire)
      expect(spy).not.toHaveBeenCalled()
  })
})

describe('ERR_PREFIX_DISPOSED', () => {
  test('prefix.call() on a disposed prefix refuses without a round trip', async () => {
    const disposed = await sandbox.prepare({ id: crypto.randomUUID(), code: 'export default () => 1' })
    await disposed.dispose()
    vi.clearAllMocks()
    const message = `prefix '${disposed.id}' has been disposed or never existed`
    await expectClientRefusal(disposed.call({ export: 'default' }), 'ERR_PREFIX_DISPOSED', message)
  })

  test('disposal wins over an already-aborted signal', async () => {
    const disposed = await sandbox.prepare({ id: crypto.randomUUID(), code: 'export default () => 1' })
    await disposed.dispose()
    vi.clearAllMocks()
    const signal = AbortSignal.abort('stop')
    await expectClientRefusal(disposed.call({ export: 'default', signal }), 'ERR_PREFIX_DISPOSED')
  })

  test('an already-aborted signal wins over the other setup checks', async () => {
    const signal = AbortSignal.abort('stop')
    expect(await prefix.call({ export: 'default', signal, globals: { zzz: async () => 1 } } as never)).toMatchObject({ status: 'aborted' })
  })
})

describe('allowed rebinds still reach the runtime', () => {
  test('a shim global rebound by name with a function runs through its shim', async () => {
    const shimmedPrefix = await sandbox.prepare({
      id: crypto.randomUUID(),
      code: `export default async () => await greet('ada')`,
      globals: { greet: { kind: 'bridge-with-shim', shim: '(s) => s.toUpperCase()', handler: async (n: string) => `default ${n}` } },
    })
    const run = await shimmedPrefix.call({ export: 'default', globals: { greet: async (n: string) => `rebound ${n}` } } as never)
    expect(run).toMatchObject({ ok: true, value: 'REBOUND ADA' })
    expect(RuntimeIpcClient.prototype.prefixRun).toHaveBeenCalled()
    await shimmedPrefix.dispose()
  })

  test('function rebinds of bridge, shim and import leaves run', async () => {
    const result = await prefix.call({
      export: 'rebinds',
      globals: { tool: async () => 'tool', shimmed: async () => 'shim' },
      imports: { 'host:tools': { add: () => 'add', nested: { fn: () => 'nested' } } },
    } as never)
    expect(result).toMatchObject({ ok: true, value: ['tool', 'shim', 'add', 'nested'] })
  })

  test('a dotted key and its nested twin dispatch and rebind separately', async () => {
    const at = async (imports?: object): Promise<unknown> => prefix.call({ export: 'dottedPair', ...(imports && { imports: { 'host:tools': { dotted: imports } } }) } as never)
    expect(await at()).toMatchObject({ ok: true, value: ['dotted', 'nested'] })
    expect(await at({ 'a.b': () => 'rebound' })).toMatchObject({ ok: true, value: ['rebound', 'nested'] })
    expect(await at({ a: { b: () => 'rebound' } })).toMatchObject({ ok: true, value: ['dotted', 'rebound'] })
  })
})

describe('the declared shape is a copy', () => {
  test('changing the caller\'s globals object after prepare() changes nothing', async () => {
    const globals: Record<string, unknown> = { tool: async () => 'default' }
    const copied = await sandbox.prepare({ id: crypto.randomUUID(), code: 'export default () => tool()', globals: globals as never })
    globals['added'] = async () => 'added'
    globals['tool'] = { kind: 'data', value: 1 }
    expect(await copied.call({ export: 'default' })).toMatchObject({ ok: true, value: 'default' })
    expect(await copied.call({ export: 'default', globals: { tool: async () => 'rebound' } } as never)).toMatchObject({ ok: true, value: 'rebound' })
    vi.clearAllMocks()
    await expectClientRefusal(copied.call({ export: 'default', globals: { added: async () => 1 } } as never), 'ERR_UNDECLARED_BINDING')
    await copied.dispose()
  })

  test('dispose() drops the declared shapes and default handlers', async () => {
    const dropped = await sandbox.prepare({ id: crypto.randomUUID(), code: '', globals: { tool: async () => 1 }, imports: { 'host:t': { f: () => 1 } } })
    await dropped.dispose()
    const internals = dropped as unknown as { declaredGlobals: Map<string, unknown>, defaultImportHandlers: Map<string, unknown>, declaredImports: unknown[] }
    expect(internals.declaredGlobals.size).toBe(0)
    expect(internals.defaultImportHandlers.size).toBe(0)
    expect(internals.declaredImports).toEqual([])
  })
})

// Each case runs through the client check, then with the client's view of the
// declared shape widened so only the runtime can refuse it. Both must agree.
describe('the client mirrors the runtime', () => {
  const fn = async (): Promise<number> => 1
  const bridge = { kind: 'bridge', handler: fn, enumerable: true }
  const fnNode = { kind: 'function' }
  const toolsModule: [string, object][] = [
    ['add', fnNode],
    ['version', { kind: 'data', value: undefined }],
    ['nested', { kind: 'object', entries: [['fn', fnNode]] }],
    ['dotted', { kind: 'object', entries: [['a.b', fnNode], ['a', { kind: 'object', entries: [['b', fnNode]] }], ['v.x', { kind: 'data', value: undefined }]] }],
    ['flat', { kind: 'object', entries: [['c.d', fnNode]] }],
    ['deep', { kind: 'object', entries: [['c', { kind: 'object', entries: [['d', fnNode]] }]] }],
  ]
  const declaredImports = [
    { specifier: 'host:tools', module: toolsModule },
    { specifier: 'lib:math', source: '' },
  ]
  const widenTools = (key: string, entries: object[]): object[] => [
    { specifier: 'host:tools', module: [...toolsModule.filter(([k]) => k !== key), [key, { kind: 'object', entries }]] },
    { specifier: 'lib:math', source: '' },
  ]
  const asFunction = (specifier: string, path: string): object[] => [
    ...declaredImports.filter((b) => b.specifier !== specifier),
    { specifier, module: [[path, { kind: 'function' }]] },
  ]
  const cases: [string, object, { globals?: string[], imports?: object[] }, string, string][] = [
    ['an undeclared global', { globals: { zzz: fn } }, { globals: ['zzz'] }, 'ERR_UNDECLARED_BINDING', `global 'zzz' was not declared at precompile time`],
    ['a runtime-owned global', { globals: { console: fn } }, { globals: ['console'] }, 'ERR_UNDECLARED_BINDING', `global 'console' was not declared at precompile time`],
    ['__proto__ as a global', { globals: Object.defineProperty({}, '__proto__', { value: fn, enumerable: true }) }, { globals: ['__proto__'] }, 'ERR_UNDECLARED_BINDING', `global '__proto__' was not declared at precompile time`],
    ['a data global', { globals: { config: fn } }, { globals: ['config'] }, 'ERR_FROZEN_BINDING', `global 'config' is a value global, frozen with the prefix — only function globals can be rebound`],
    ['a string global', { globals: { expr: fn } }, { globals: ['expr'] }, 'ERR_FROZEN_BINDING', `global 'expr' is a value global, frozen with the prefix — only function globals can be rebound`],
    ['an undeclared specifier', { imports: { 'host:nope': { f: fn } } }, { imports: [...declaredImports, { specifier: 'host:nope', module: [['f', { kind: 'function' }]] }] }, 'ERR_UNDECLARED_BINDING', `import 'host:nope' was not declared at precompile time`],
    ['an undeclared path', { imports: { 'host:tools': { nope: fn } } }, { imports: asFunction('host:tools', 'nope') }, 'ERR_UNDECLARED_BINDING', `import 'host:tools'.nope was not declared at precompile time`],
    ['a data leaf', { imports: { 'host:tools': { version: fn } } }, { imports: asFunction('host:tools', 'version') }, 'ERR_FROZEN_BINDING', `import 'host:tools'.version is a data leaf, not a function — data leaves cannot be rebound`],
    ['a source module', { imports: { 'lib:math': { x: fn } } }, { imports: asFunction('lib:math', 'x') }, 'ERR_FROZEN_BINDING', `import 'lib:math' is a source module — source imports are frozen with the prefix and cannot be rebound at prefix.call() time`],
    ['a dotted key rebound as nesting', { imports: { 'host:tools': { flat: { c: { d: fn } } } } }, { imports: widenTools('flat', [['c.d', fnNode], ['c', { kind: 'object', entries: [['d', fnNode]] }]]) }, 'ERR_UNDECLARED_BINDING', `import 'host:tools'.flat.c was not declared at precompile time`],
    ['a nested path rebound as a dotted key', { imports: { 'host:tools': { deep: { 'c.d': fn } } } }, { imports: widenTools('deep', [['c', { kind: 'object', entries: [['d', fnNode]] }], ['c.d', fnNode]]) }, 'ERR_UNDECLARED_BINDING', `import 'host:tools'.deep.c.d was not declared at precompile time`],
    ['a data leaf under a dotted key', { imports: { 'host:tools': { dotted: { 'v.x': fn } } } }, { imports: widenTools('dotted', [['v.x', fnNode]]) }, 'ERR_FROZEN_BINDING', `import 'host:tools'.dotted.v.x is a data leaf, not a function — data leaves cannot be rebound`],
  ]

  test.each(cases)('%s', async (_kind, options, widen, code, message) => {
    const client = await call(options).then(() => undefined, (e: unknown) => e)
    expect(client).toMatchObject({ name: 'Error', code, message })
    expect(RuntimeIpcClient.prototype.prefixRun).not.toHaveBeenCalled()

    const widened = await sandbox.prepare({
      id: crypto.randomUUID(),
      code: 'export default () => 1',
      globals: { config: { kind: 'data', value: 1 }, expr: '1' },
      imports: {
        'host:tools': {
          add: (n: number) => n + 1,
          version: '1.0.0',
          nested: { fn: () => 1 },
          dotted: { 'a.b': () => 1, 'a': { b: () => 1 }, 'v.x': '1' },
          flat: { 'c.d': () => 1 },
          deep: { c: { d: () => 1 } },
        },
        'lib:math': 'export const x = 1',
      },
    })
    const internals = widened as unknown as { declaredGlobals: Map<string, unknown>, declaredImports: object[] }
    internals.declaredGlobals = new Map((widen.globals ?? []).map((name) => [name, bridge]))
    if (widen.imports)
      internals.declaredImports = widen.imports
    const runtime = await widened.call({ export: 'default', ...options } as never).then(() => undefined, (e: unknown) => e)
    expect(RuntimeIpcClient.prototype.prefixRun).toHaveBeenCalledTimes(1)
    expect(Object.getPrototypeOf(runtime)).toBe(Error.prototype)
    expect(Object.keys(runtime as object)).toEqual(Object.keys(client as object))
    expect(runtime).toMatchObject({ name: 'Error', code, message })
    await widened.dispose()
  })

  test('a prefix the runtime dropped', async () => {
    const ghost = await sandbox.prepare({ id: crypto.randomUUID(), code: 'export default () => 1' })
    const local = await sandbox.prepare({ id: crypto.randomUUID(), code: 'export default () => 1' })
    await (ghost as unknown as { pool: RunPool }).pool.withClient((c) => c.disposePrefix(ghost.id))
    await local.dispose()
    // The runtime cannot tell a dropped prefix from one it never held: a failed result.
    const runtime = await ghost.call({ export: 'default' })
    const client = await local.call({ export: 'default' }).then(() => undefined, (e: unknown) => e)
    expect(runtime).toMatchObject({ ok: false, error: { name: 'Error', code: 'ERR_PREFIX_UNKNOWN', message: `prefix '${ghost.id}' is unknown to the runtime: never prepared, disposed, or prepared with other code` } })
    expect(client).toMatchObject({ name: 'Error', code: 'ERR_PREFIX_DISPOSED', message: `prefix '${local.id}' has been disposed or never existed` })
    await ghost.dispose()
  })

  test('a prefix run with another code hash', async () => {
    const prefix = await sandbox.prepare({ id: crypto.randomUUID(), code: 'export default () => 1' })
    const internals = prefix as unknown as { hash: Uint8Array }
    const hash = internals.hash
    internals.hash = new Uint8Array(16)
    const runtime = await prefix.call({ export: 'default' })
    expect(runtime).toMatchObject({ ok: false, error: { code: 'ERR_PREFIX_UNKNOWN' } })
    internals.hash = hash
    expect(await prefix.call({ export: 'default' })).toMatchObject({ ok: true, value: 1 })
    await prefix.dispose()
  })
})

describe('prefix ids', () => {
  const realPrecompile = RuntimeIpcClient.prototype.precompile
  const realDisposePrefix = RuntimeIpcClient.prototype.disposePrefix

  test('prepare() throws a TypeError for a missing or bad id, without a round trip', async () => {
    for (const id of [undefined, null, 5, '', 'x'.repeat(201), 'é'.repeat(101), 'a\uD800b']) {
      await expect(sandbox.prepare({ id: id as never, code: 'export default 1' })).rejects.toThrow(TypeError)
    }
    for (const spy of wire)
      expect(spy).not.toHaveBeenCalled()
  })

  test('any well-formed string up to 200 bytes is an id, kept as given', async () => {
    for (const id of ['é'.repeat(100), 'line\nbreak "quoted"', ' ', '🦀']) {
      const p = await sandbox.prepare({ id, code: 'export default () => 1' })
      expect(p.id).toBe(id)
      expect(await p.call({ export: 'default' })).toMatchObject({ ok: true, value: 1 })
      await p.dispose()
    }
  })

  test('the same id and code is the same prefix until its last handle is disposed', async () => {
    const id = crypto.randomUUID()
    const code = 'let n = 0; export default () => ++n'
    const a = await sandbox.prepare({ id, code })
    const b = await sandbox.prepare({ id, code })
    expect(b).not.toBe(a)
    expect([a.id, b.id]).toEqual([id, id])
    expect(await a.call({ export: 'default' })).toMatchObject({ ok: true })
    await a.dispose()
    await a.dispose() // a handle disposes once
    expect(await b.call({ export: 'default' })).toMatchObject({ ok: true })
    await expect(sandbox.prepare({ id, code: 'export default () => 2' })).rejects.toMatchObject({ code: 'ERR_PREFIX_ID_TAKEN' })
    await b.dispose()
    const c = await sandbox.prepare({ id, code: 'export default () => 2' })
    expect(await c.call({ export: 'default' })).toMatchObject({ ok: true, value: 2 })
    await c.dispose()
  })

  test('other code under a held id throws ERR_PREFIX_ID_TAKEN', async () => {
    const id = 'taken "id"'
    const held = await sandbox.prepare({ id, code: 'export default () => 1' })
    const error = await sandbox.prepare({ id, code: 'export default () => 2' }).then(() => undefined, (e: unknown) => e)
    expect(Object.getPrototypeOf(error)).toBe(Error.prototype)
    expect(error).toMatchObject({ name: 'Error', code: 'ERR_PREFIX_ID_TAKEN', message: 'prefix id "taken "id"" is held with other code' })
    expect(await held.call({ export: 'default' })).toMatchObject({ ok: true, value: 1 })
    await held.dispose()
  })

  test('other memory limits under a held id throw ERR_PREFIX_ID_TAKEN naming both', async () => {
    const id = crypto.randomUUID()
    const code = 'export default () => 1'
    const held = await sandbox.prepare({ id, code, memoryMb: { soft: 64, hard: 96 } })
    await expect(sandbox.prepare({ id, code, memoryMb: { soft: 80, hard: 96 } })).rejects.toMatchObject({
      code: 'ERR_PREFIX_ID_TAKEN',
      message: `prefix id "${id}" is held with other memory limits (held: memoryMb 64, asked: memoryMb 80)`,
    })
    await expect(sandbox.prepare({ id, code, memoryMb: { soft: 64, hard: 128 } })).rejects.toMatchObject({
      code: 'ERR_PREFIX_ID_TAKEN',
      message: `prefix id "${id}" is held with other memory limits (held: memoryMb 64, hardMemoryMb 96, asked: memoryMb 64, hardMemoryMb 128)`,
    })
    // A refused prepare holds nothing: the first handle's dispose still drops the prefix.
    await held.dispose()
    const again = await sandbox.prepare({ id, code, memoryMb: 32 })
    expect(await again.call({ export: 'default' })).toMatchObject({ ok: true, value: 1 })
    await again.dispose()
  })

  test('after await dispose() the id takes other code at once, every time', async () => {
    const id = crypto.randomUUID()
    for (let i = 0; i < 20; i++) {
      const p = await sandbox.prepare({ id, code: `export default () => ${i}` })
      expect(await p.call({ export: 'default' })).toMatchObject({ ok: true, value: i })
      await p.dispose()
    }
  })

  test('a prepare started while the dispose of its id is pending waits for the answer', async () => {
    const id = crypto.randomUUID()
    const order: string[] = []
    vi.spyOn(RuntimeIpcClient.prototype, 'disposePrefix').mockImplementation(async function (this: RuntimeIpcClient, prefixId: string) {
      order.push('dispose sent')
      await new Promise((resolve) => {
        setTimeout(resolve, 50)
      })
      const answered = await realDisposePrefix.call(this, prefixId)
      order.push('dispose answered')
      return answered
    })
    vi.spyOn(RuntimeIpcClient.prototype, 'precompile').mockImplementation(async function (this: RuntimeIpcClient, options) {
      order.push(`precompile ${options.code}`)
      return realPrecompile.call(this, options)
    })
    const old = await sandbox.prepare({ id, code: 'export default () => 1' })
    const disposing = old.dispose()
    const next = await sandbox.prepare({ id, code: 'export default () => 2' })
    await disposing
    expect(order).toEqual([
      'precompile export default () => 1',
      'dispose sent',
      'dispose answered',
      'precompile export default () => 2',
    ])
    expect(await next.call({ export: 'default' })).toMatchObject({ ok: true, value: 2 })
    await next.dispose()
  })

  test('a prepare whose reply is lost still disposes what the runtime stored', async () => {
    const id = crypto.randomUUID()
    vi.spyOn(RuntimeIpcClient.prototype, 'precompile').mockImplementationOnce(async function (this: RuntimeIpcClient, options) {
      await realPrecompile.call(this, options)
      throw new Error('reply lost')
    })
    await expect(sandbox.prepare({ id, code: 'export default () => 1' })).rejects.toThrow('reply lost')
    const next = await sandbox.prepare({ id, code: 'export default () => 2' })
    expect(await next.call({ export: 'default' })).toMatchObject({ ok: true, value: 2 })
    await next.dispose()
  })

  test('a dispose that got no answer is sent once more while the sandbox lives', async () => {
    const id = crypto.randomUUID()
    const old = await sandbox.prepare({ id, code: 'export default () => 1' })
    // The connection ended before the runtime read the DisposePrefix: it still holds the prefix.
    const drop = vi.spyOn(RuntimeIpcClient.prototype, 'disposePrefix').mockImplementationOnce(async () => false)
    await old.dispose()
    expect(drop).toHaveBeenCalledTimes(2)
    const next = await sandbox.prepare({ id, code: 'export default () => 2' })
    expect(drop).toHaveBeenCalledTimes(2)
    expect(await next.call({ export: 'default' })).toMatchObject({ ok: true, value: 2 })
    await next.dispose()
  })

  test('dispose() resolves when the sandbox closes or the child dies while it waits for the answer', async () => {
    for (const end of ['sandbox dispose', 'child killed'] as const) {
      const sb = await createSandbox()
      const p = await sb.prepare({ id: 'x', code: 'export default () => 1' })
      const sent = vi.spyOn(RuntimeIpcClient.prototype, 'disposePrefix')
      const disposed = p.dispose()
      await vi.waitFor(() => expect(sent).toHaveBeenCalled())
      if (end === 'child killed')
        (sb as unknown as { proc: { kill: (signal: string) => void } }).proc.kill('SIGKILL')
      else
        await sb.dispose()
      await expect(disposed).resolves.toBeUndefined()
      // A sandbox going down gets no second dispose.
      if (end === 'sandbox dispose')
        expect(sent).toHaveBeenCalledTimes(1)
      sent.mockRestore()
      await sb.dispose()
    }
  })

  test('an unanswered dispose is not sent again once the sandbox is going down', async () => {
    const sb = await createSandbox()
    const p = await sb.prepare({ id: 'x', code: 'export default () => 1' })
    let closing!: Promise<void>
    const drop = vi.spyOn(RuntimeIpcClient.prototype, 'disposePrefix').mockImplementationOnce(async () => {
      closing = sb.dispose()
      return false
    })
    await expect(p.dispose()).resolves.toBeUndefined()
    await closing
    expect(drop).toHaveBeenCalledTimes(1)
  })

  test('dispose() still rejects when the sandbox was closed before it could send', async () => {
    const sb = await createSandbox()
    const p = await sb.prepare({ id: 'x', code: 'export default () => 1' })
    const disposed = p.dispose().then(() => undefined, (e: unknown) => e)
    await sb.dispose()
    expect(await disposed).toMatchObject({ message: 'runtime is disposed' })
  })

  test('preparing a broken id with the same code again gives it a fresh attempt', async () => {
    const id = crypto.randomUUID()
    // Fails its first cold start only: the clock passes `until` before the rebuild.
    const until = Date.now() + 300
    const code = `if (Date.now() < ${until}) throw new Error('not yet'); export default () => 1`
    const first = await sandbox.prepare({ id, code })
    expect(await first.call({ export: 'default' })).toMatchObject({ ok: false, error: { stage: 'prefix', message: 'not yet' } })
    expect(first.status).toBe('broken')
    await new Promise((resolve) => {
      setTimeout(resolve, Math.max(0, until - Date.now()) + 50)
    })
    const second = await sandbox.prepare({ id, code })
    expect(await second.call({ export: 'default' })).toMatchObject({ ok: true, value: 1 })
    // The first handle keeps its own broken status and refuses without a round trip.
    await expect(first.call({ export: 'default' })).rejects.toMatchObject({ code: 'ERR_PREFIX_BROKEN' })
    await first.dispose()
    expect(await second.call({ export: 'default' })).toMatchObject({ ok: true, value: 1 })
    await second.dispose()
  })
})
