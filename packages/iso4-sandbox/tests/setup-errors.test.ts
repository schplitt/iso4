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
    code: 'export default () => 1',
    globals: {
      tool: async () => 1,
      shimmed: { kind: 'bridge-with-shim', shim: '(h) => h', handler: async () => 1 },
      config: { kind: 'data', value: 1 },
      expr: '1',
    },
    imports: {
      'host:tools': { add: (n: number) => n + 1, version: '1.0.0', nested: { fn: () => 1 } },
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
const run = (options: object): Promise<unknown> => prefix.run({ code: 'export default 1', ...options } as never)

describe('ERR_RESERVED_NAME', () => {
  test.each(['console', 'Response', 'NaN', '__proto__'])('run() and prepare() refuse %s without a round trip', async (name) => {
    const message = `global '${name}' is reserved by the runtime and cannot be provided by the host`
    await expectClientRefusal(sandbox.run({ code: 'export default 1', globals: { [name]: { kind: 'data', value: 1 } } as never }), 'ERR_RESERVED_NAME', message)
    await expectClientRefusal(sandbox.prepare({ code: '', globals: { [name]: async () => 1 } as never }), 'ERR_RESERVED_NAME', message)
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
  ])('prefix.run() and prefix.call() refuse %s without a round trip', async (_kind, options, message) => {
    await expectClientRefusal(run(options), 'ERR_UNDECLARED_BINDING', message)
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
  ])('prefix.run() and prefix.call() refuse %s without a round trip', async (_kind, options, message) => {
    await expectClientRefusal(run(options), 'ERR_FROZEN_BINDING', message)
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
  ])('prefix.run() and prefix.call() refuse %s without a round trip', async (_kind, options, message) => {
    await expectClientRefusal(run(options), 'ERR_INVALID_REBIND', message)
    await expectClientRefusal(call(options), 'ERR_INVALID_REBIND', message)
  })
})

describe('malformed options', () => {
  test.each(['run', 'call'] as const)('prefix.%s() throws a TypeError for a bad limit without a round trip', async (kind) => {
    const promise = kind === 'run' ? run({ limits: { cpuTimeMs: -1 } }) : call({ limits: { cpuTimeMs: 1.5 } })
    await expect(promise).rejects.toThrow(TypeError)
    for (const spy of wire)
      expect(spy).not.toHaveBeenCalled()
  })
})

describe('ERR_PREFIX_DISPOSED', () => {
  test('prefix.run() and prefix.call() on a disposed prefix refuse without a round trip', async () => {
    const disposed = await sandbox.prepare({ code: 'export default () => 1' })
    await disposed.dispose()
    vi.clearAllMocks()
    const message = `prefix '${disposed.id}' has been disposed or never existed`
    await expectClientRefusal(disposed.run({ code: 'export default 1' }), 'ERR_PREFIX_DISPOSED', message)
    await expectClientRefusal(disposed.call({ export: 'default' }), 'ERR_PREFIX_DISPOSED', message)
  })

  test('disposal wins over an already-aborted signal', async () => {
    const disposed = await sandbox.prepare({ code: 'export default () => 1' })
    await disposed.dispose()
    vi.clearAllMocks()
    const signal = AbortSignal.abort('stop')
    await expectClientRefusal(disposed.run({ code: 'export default 1', signal }), 'ERR_PREFIX_DISPOSED')
    await expectClientRefusal(disposed.call({ export: 'default', signal }), 'ERR_PREFIX_DISPOSED')
  })

  test('an already-aborted signal wins over the other setup checks', async () => {
    const signal = AbortSignal.abort('stop')
    expect(await prefix.run({ code: 'export default 1', signal, globals: { zzz: async () => 1 } } as never)).toMatchObject({ status: 'aborted' })
  })
})

describe('allowed rebinds still reach the runtime', () => {
  test('a shim global rebound by name with a function runs through its shim', async () => {
    const shimmedPrefix = await sandbox.prepare({
      code: '',
      globals: { greet: { kind: 'bridge-with-shim', shim: '(s) => s.toUpperCase()', handler: async (n: string) => `default ${n}` } },
    })
    const run = await shimmedPrefix.run({ code: `export default await greet('ada')`, globals: { greet: async (n: string) => `rebound ${n}` } } as never)
    expect(run).toMatchObject({ ok: true, exports: { default: 'REBOUND ADA' } })
    expect(RuntimeIpcClient.prototype.prefixRun).toHaveBeenCalled()
    await shimmedPrefix.dispose()
  })

  test('function rebinds of bridge, shim and import leaves run', async () => {
    const result = await prefix.run({
      code: `import { add, nested } from 'host:tools'\nexport default [await tool(), await shimmed(), await add(1), await nested.fn()]`,
      globals: { tool: async () => 'tool', shimmed: async () => 'shim' },
      imports: { 'host:tools': { add: () => 'add', nested: { fn: () => 'nested' } } },
    } as never)
    expect(result).toMatchObject({ ok: true, exports: { default: ['tool', 'shim', 'add', 'nested'] } })
  })
})

describe('the declared shape is a copy', () => {
  test('changing the caller\'s globals object after prepare() changes nothing', async () => {
    const globals: Record<string, unknown> = { tool: async () => 'default' }
    const copied = await sandbox.prepare({ code: 'export default () => tool()', globals: globals as never })
    globals['added'] = async () => 'added'
    globals['tool'] = { kind: 'data', value: 1 }
    expect(await copied.call({ export: 'default' })).toMatchObject({ ok: true, value: 'default' })
    expect(await copied.call({ export: 'default', globals: { tool: async () => 'rebound' } } as never)).toMatchObject({ ok: true, value: 'rebound' })
    vi.clearAllMocks()
    await expectClientRefusal(copied.call({ export: 'default', globals: { added: async () => 1 } } as never), 'ERR_UNDECLARED_BINDING')
    await copied.dispose()
  })

  test('dispose() drops the declared shapes and default handlers', async () => {
    const dropped = await sandbox.prepare({ code: '', globals: { tool: async () => 1 }, imports: { 'host:t': { f: () => 1 } } })
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
  const declaredImports = [
    { specifier: 'host:tools', module: [['add', { kind: 'function' }], ['version', { kind: 'data', value: undefined }], ['nested', { kind: 'object', entries: [['fn', { kind: 'function' }]] }]] },
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
    ['a source module', { imports: { 'lib:math': { x: fn } } }, { imports: asFunction('lib:math', 'x') }, 'ERR_FROZEN_BINDING', `import 'lib:math' is a source module — source imports are frozen with the prefix and cannot be rebound at prefix.run() time`],
  ]

  test.each(cases)('%s', async (_kind, options, widen, code, message) => {
    const client = await call(options).then(() => undefined, (e: unknown) => e)
    expect(client).toMatchObject({ name: 'Error', code, message })
    expect(RuntimeIpcClient.prototype.prefixRun).not.toHaveBeenCalled()

    const widened = await sandbox.prepare({
      code: 'export default () => 1',
      globals: { config: { kind: 'data', value: 1 }, expr: '1' },
      imports: { 'host:tools': { add: (n: number) => n + 1, version: '1.0.0', nested: { fn: () => 1 } }, 'lib:math': 'export const x = 1' },
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
    const ghost = await sandbox.prepare({ code: 'export default () => 1' })
    const local = await sandbox.prepare({ code: 'export default () => 1' })
    await (ghost as unknown as { pool: RunPool }).pool.withClient((c) => c.disposePrefix(ghost.id))
    await local.dispose()
    const runtime = await ghost.call({ export: 'default' }).then(() => undefined, (e: unknown) => e)
    const client = await local.call({ export: 'default' }).then(() => undefined, (e: unknown) => e)
    expect(runtime).toMatchObject({ name: 'Error', code: 'ERR_PREFIX_DISPOSED', message: `prefix '${ghost.id}' has been disposed or never existed` })
    expect(client).toMatchObject({ name: 'Error', code: 'ERR_PREFIX_DISPOSED', message: `prefix '${local.id}' has been disposed or never existed` })
    await ghost.dispose()
  })
})
