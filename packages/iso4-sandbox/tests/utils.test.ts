import { afterAll, beforeAll, describe, expect, it, vi } from 'vitest'
import { createSandbox } from '../src/index.js'
import type { Sandbox } from '../src/types.js'
import { RUNTIME_OWNED_GLOBALS } from '../src/names.js'
import { IDENTIFIER_NAME, isValidExportName, isValidGlobalName, RESERVED_WORDS, SANDBOX_GLOBALS } from '../src/utils.js'

const CANDIDATES = ['myTool', 'café', 'π', '$x', '_y', 'a‌b', 'my-tool', '1abc', '', 'a b', 'class', 'true', 'default', 'eval', 'arguments', 'console', 'Response', 'Math', 'undefined', '__proto__', '℘']

describe('isValidExportName', () => {
  it.each(['myTool', 'café', 'π', '$x', '_y', 'a‌b', 'Math', 'console', 'undefined', 'default'])('accepts %s', (name) => {
    expect(isValidExportName(name)).toBe(true)
  })

  it.each(['my-tool', '1abc', '', 'a b', 'class', 'true', 'eval', 'arguments'])('rejects %s', (name) => {
    expect(isValidExportName(name)).toBe(false)
  })

  it('is IDENTIFIER_NAME minus RESERVED_WORDS, plus default', () => {
    for (const name of [...CANDIDATES, ...RESERVED_WORDS, ...SANDBOX_GLOBALS])
      expect(isValidExportName(name)).toBe(name === 'default' || (IDENTIFIER_NAME.test(name) && !RESERVED_WORDS.has(name)))
  })
})

describe('isValidGlobalName', () => {
  it.each(['myTool', 'café', 'π', '$x', 'eval', 'arguments', 'Math', 'my-tool', '1abc', '', 'a b', 'class', 'true', 'default'])('accepts %j', (name) => {
    expect(isValidGlobalName(name)).toBe(true)
  })

  it.each(['console', 'setTimeout', 'Response', 'URL', 'NaN', 'undefined', 'Infinity', '__proto__', 'y\uD800', '\uDC00'])('rejects %j', (name) => {
    expect(isValidGlobalName(name)).toBe(false)
  })
})

describe('the predicates agree with the runtime', () => {
  let sandbox: Sandbox

  beforeAll(async () => {
    sandbox = await createSandbox()
  }, 20_000)

  afterAll(async () => {
    await sandbox?.dispose()
  })

  const globalCode = (name: string): string => isValidGlobalName(name) ? '' : 'ERR_RESERVED_NAME'

  // The runtime's own answer: the client's reserved-name check is switched off for the call.
  async function runtimeCode(name: string, value: unknown): Promise<string | undefined> {
    const spy = vi.spyOn(RUNTIME_OWNED_GLOBALS as Set<string>, 'has').mockReturnValue(false)
    try {
      const globals = Object.defineProperty({}, name, { value, enumerable: true })
      return await sandbox.run({ code: 'export default 1', globals })
        .then((result) => result.ok ? '' : result.error.code, (error: { code?: string }) => error.code)
    } finally {
      spy.mockRestore()
    }
  }

  it.each(CANDIDATES.filter((name) => name !== 'default'))('compiles export const %s iff isValidExportName', async (name) => {
    const result = await sandbox.run({ code: `export const ${name} = 1` })
    expect(result.ok).toBe(isValidExportName(name))
  })

  it('does not compile export const default, which isValidExportName accepts as a host-module key', async () => {
    expect((await sandbox.run({ code: 'export const default = 1' })).ok).toBe(false)
  })

  it.each(CANDIDATES)('prepare() takes host-module export %s iff isValidExportName', async (name) => {
    const prepared = sandbox.prepare({ code: '', imports: { 'host:t': { [name]: () => 1 } } })
    if (isValidExportName(name))
      await expect(prepared).resolves.toBeDefined()
    else
      await expect(prepared).rejects.toThrow(/is not a valid JavaScript identifier/)
  })

  it('a default host-module key is the module\'s default export', async () => {
    const prefix = await sandbox.prepare({ code: '', imports: { 'host:t': { default: () => 7 } } })
    const result = await prefix.execute({ code: `import t from 'host:t'\nexport default await t()` })
    expect(result.ok && result.exports['default']).toBe(7)
  })

  it.each(CANDIDATES)('prepare() takes global %s iff isValidGlobalName', async (name) => {
    const prepared = sandbox.prepare({ code: '', globals: { [name]: { kind: 'data', value: 1 } } as never })
    if (isValidGlobalName(name))
      await expect(prepared).resolves.toBeDefined()
    else
      await expect(prepared).rejects.toMatchObject({ code: globalCode(name) })
  })

  it('prepare() refuses a global name that is not a well-formed string, like isValidGlobalName', async () => {
    expect(isValidGlobalName('y\uD800')).toBe(false)
    await expect(sandbox.prepare({ code: '', globals: { 'y\uD800': { kind: 'data', value: 1 } } }))
      .rejects
      .toThrow(TypeError)
  })

  it('the runtime refuses every name in the runtime-owned copy', async () => {
    for (const name of RUNTIME_OWNED_GLOBALS)
      expect(await runtimeCode(name, { kind: 'data', value: 1 }), name).toBe('ERR_RESERVED_NAME')
  })

  it.each([
    ['data', { kind: 'data', value: 1 }],
    ['function', async () => 1],
  ] as const)('run() takes every SANDBOX_GLOBALS name as a %s global iff isValidGlobalName', async (kind, value) => {
    for (const name of SANDBOX_GLOBALS)
      expect(await runtimeCode(name, value), name).toBe(globalCode(name))
  })

  it.each(['NaN', 'undefined', 'Infinity', '__proto__'])('prepare() and run() refuse %s as a global of every kind with ERR_RESERVED_NAME', async (name) => {
    const values = [
      { kind: 'data', value: 1 },
      { kind: 'data', value: 1, enumerable: false },
      '1',
      { kind: 'string', expr: '1', enumerable: false },
      async () => 1,
      { kind: 'bridge', handler: async () => 1, enumerable: false },
      { kind: 'bridge', handler: async () => 1 },
      { kind: 'bridge-with-shim', shim: '(h) => h', handler: async () => 1 },
      { kind: 'bridge-with-shim', shim: '(h) => h', handler: async () => 1, enumerable: false },
    ]
    for (const value of values) {
      await expect(sandbox.prepare({ code: '', globals: { [name]: value } as never })).rejects.toMatchObject({ code: 'ERR_RESERVED_NAME' })
      await expect(sandbox.run({ code: 'export default 1', globals: { [name]: value } as never })).rejects.toMatchObject({ code: 'ERR_RESERVED_NAME' })
      expect(await runtimeCode(name, value)).toBe('ERR_RESERVED_NAME')
    }
  })
})
