/**
 * Tests for the HostGlobalValue processing layer.
 *
 * Two categories:
 * 1. Unit tests — processGlobals / extractBridgeGlobals, no binary needed.
 * 2. Type tests — RebindGlobals constraints verified with \@ts-expect-error.
 *
 * End-to-end shim behaviour (sandbox actually calling the shimmed global) is
 * covered in integration.test.ts under "globals bridge — BridgeWithShim".
 */

import { describe, expect, it } from 'vitest'
import type {
  BridgeWithShim,
  DataGlobal,
  HostExportFunction,
  HostGlobals,
  RebindGlobals,
} from '../src/types.js'
import type { GlobalDefPayload } from '../src/ipc.js'
import { extractBridgeGlobals, processGlobals } from '../src/globals.js'

// Find a produced global def by its public name.
function defFor(defs: GlobalDefPayload[], name: string): GlobalDefPayload | undefined {
  return defs.find((d) => d.name === name)
}

// ─────────────────────────────────────────────────────────────────────────
// processGlobals — structured defs + bridge dispatch map
// ─────────────────────────────────────────────────────────────────────────

describe('processGlobals', () => {
  describe('HostExportFunction', () => {
    it('emits a bridge def and a dispatch entry under its own name', () => {
      const fn: HostExportFunction = () => 42
      const { defs, dispatch } = processGlobals({ myTool: fn })
      expect(defFor(defs, 'myTool')).toEqual({ kind: 'bridge', name: 'myTool' })
      expect(dispatch['myTool']).toBe(fn)
    })

    it('registers multiple plain functions independently', () => {
      const a: HostExportFunction = () => 'a'
      const b: HostExportFunction = () => 'b'
      const { defs, dispatch } = processGlobals({ a, b })
      expect(defFor(defs, 'a')).toEqual({ kind: 'bridge', name: 'a' })
      expect(defFor(defs, 'b')).toEqual({ kind: 'bridge', name: 'b' })
      expect(dispatch['a']).toBe(a)
      expect(dispatch['b']).toBe(b)
    })
  })

  describe('string global', () => {
    it('emits a string-expr def and no dispatch entry', () => {
      const { defs, dispatch } = processGlobals({ PI: `Math.PI` })
      expect(defFor(defs, 'PI')).toEqual({ kind: 'string', name: 'PI', expr: 'Math.PI' })
      expect(Object.keys(dispatch)).toHaveLength(0)
    })

    it('carries the raw expression verbatim (no globalThis wrapping in the client)', () => {
      const { defs } = processGlobals({ VERSION: `'2.0'` })
      expect(defFor(defs, 'VERSION')).toEqual({ kind: 'string', name: 'VERSION', expr: `'2.0'` })
    })

    it('multiple string globals produce multiple defs', () => {
      const { defs } = processGlobals({ A: `1`, B: `2` })
      expect(defFor(defs, 'A')).toEqual({ kind: 'string', name: 'A', expr: '1' })
      expect(defFor(defs, 'B')).toEqual({ kind: 'string', name: 'B', expr: '2' })
    })
  })

  describe('data global', () => {
    it('emits a data def carrying the value, no dispatch entry', () => {
      const value = { model: 'gpt-4', maxTokens: 1000 }
      const dataGlobal: DataGlobal = { kind: 'data', value }
      const { defs, dispatch } = processGlobals({ config: dataGlobal })
      expect(defFor(defs, 'config')).toEqual({ kind: 'data', name: 'config', value })
      expect(Object.keys(dispatch)).toHaveLength(0)
    })
  })

  describe('BridgeWithShim', () => {
    const handler: HostExportFunction = async () => ({ status: 200, body: null })
    const shim = `(result) => ({ ...result, ok: result.status < 300 })`

    const shimGlobal: BridgeWithShim = { kind: 'bridge-with-shim', handler, shim }

    it('dispatches the handler under the private __iso4_<name>_h key, not the public name', () => {
      const { dispatch } = processGlobals({ fetch: shimGlobal })
      expect(dispatch['fetch']).toBeUndefined()
      expect(dispatch['__iso4_fetch_h']).toBe(handler)
    })

    it('emits a shim def naming the shim expression and the private handler', () => {
      const { defs } = processGlobals({ fetch: shimGlobal })
      expect(defFor(defs, 'fetch')).toEqual({
        kind: 'shim',
        name: 'fetch',
        shim,
        handlerName: '__iso4_fetch_h',
      })
    })

    it('uses the correct private key for any global name', () => {
      const { defs, dispatch } = processGlobals({ myApi: shimGlobal })
      expect(dispatch['__iso4_myApi_h']).toBe(handler)
      expect(defFor(defs, 'myApi')).toMatchObject({ kind: 'shim', handlerName: '__iso4_myApi_h' })
    })

    it('mixes plain functions, string globals, data globals, and shims correctly', () => {
      const plainFn: HostExportFunction = () => 'plain'
      const { defs, dispatch } = processGlobals({
        fetch: shimGlobal, // BridgeWithShim
        myTool: plainFn, // HostExportFunction
        API_URL: `'https://api'`, // string
        limits: { kind: 'data', value: { max: 5 } }, // DataGlobal
      })

      // Dispatch map: plain function under own name, shim under private key.
      expect(dispatch['myTool']).toBe(plainFn)
      expect(dispatch['__iso4_fetch_h']).toBe(handler)
      expect(dispatch['fetch']).toBeUndefined()
      expect(dispatch['API_URL']).toBeUndefined()
      expect(dispatch['limits']).toBeUndefined()

      // One def per global, tagged by kind.
      expect(defFor(defs, 'myTool')).toMatchObject({ kind: 'bridge' })
      expect(defFor(defs, 'fetch')).toMatchObject({ kind: 'shim' })
      expect(defFor(defs, 'API_URL')).toMatchObject({ kind: 'string' })
      expect(defFor(defs, 'limits')).toMatchObject({ kind: 'data' })
    })
  })
})

// ─────────────────────────────────────────────────────────────────────────
// extractBridgeGlobals — rebinding for prefix.run()
// ─────────────────────────────────────────────────────────────────────────

describe('extractBridgeGlobals', () => {
  const defaultHandler: HostExportFunction = async () => ({ status: 200, body: 'default' })
  const shimGlobal: BridgeWithShim = {
    kind: 'bridge-with-shim',
    handler: defaultHandler,
    shim: `(r) => r`,
  }
  const plainFn: HostExportFunction = () => 'plain'

  const precompileGlobals: HostGlobals = {
    fetch: shimGlobal,
    myTool: plainFn,
    API_URL: `'https://api'`,
    config: { kind: 'data', value: { a: 1 } },
  }

  it('string and data globals are skipped — they are baked into the snapshot', () => {
    const { defs, dispatch } = extractBridgeGlobals({}, precompileGlobals)
    expect(defFor(defs, 'API_URL')).toBeUndefined()
    expect(defFor(defs, 'config')).toBeUndefined()
    expect(dispatch['API_URL']).toBeUndefined()
    expect(dispatch['config']).toBeUndefined()
  })

  it('every re-installed def is a plain bridge stub', () => {
    const { defs } = extractBridgeGlobals({}, precompileGlobals)
    for (const d of defs) expect(d.kind).toBe('bridge')
  })

  it('shimmed global with no override re-installs the private handler stub with the default', () => {
    const { defs, dispatch } = extractBridgeGlobals({}, precompileGlobals)
    expect(defFor(defs, '__iso4_fetch_h')).toEqual({ kind: 'bridge', name: '__iso4_fetch_h' })
    expect(dispatch['__iso4_fetch_h']).toBe(defaultHandler)
    expect(dispatch['fetch']).toBeUndefined()
  })

  it('plain function with no override uses the precompile handler at own name', () => {
    const { defs, dispatch } = extractBridgeGlobals({}, precompileGlobals)
    expect(defFor(defs, 'myTool')).toEqual({ kind: 'bridge', name: 'myTool' })
    expect(dispatch['myTool']).toBe(plainFn)
  })

  it('shimmed global with function override: override goes to the private key', () => {
    const newHandler: HostExportFunction = async () => ({ status: 418, body: 'teapot' })
    const { dispatch } = extractBridgeGlobals({ fetch: newHandler }, precompileGlobals)
    expect(dispatch['__iso4_fetch_h']).toBe(newHandler)
    expect(dispatch['fetch']).toBeUndefined()
  })

  it('plain function with override: override goes to own name', () => {
    const newTool: HostExportFunction = () => 'overridden'
    const { dispatch } = extractBridgeGlobals({ myTool: newTool }, precompileGlobals)
    expect(dispatch['myTool']).toBe(newTool)
  })

  it('produces a dispatch map of only functions', () => {
    const { dispatch } = extractBridgeGlobals({}, precompileGlobals)
    for (const v of Object.values(dispatch)) {
      expect(typeof v).toBe('function')
    }
  })
})

// ─────────────────────────────────────────────────────────────────────────
// Type-level constraints — RebindGlobals
// ─────────────────────────────────────────────────────────────────────────

describe('RebindGlobals type constraints (compile-time)', () => {
  type SafeFetchFn = (...args: unknown[]) => Promise<{ status: number, body: unknown }>
  type MyToolFn = HostExportFunction

  interface G {
    fetch: BridgeWithShim<SafeFetchFn>
    myTool: MyToolFn
    API_URL: string
    config: DataGlobal
  }

  type Rebind = RebindGlobals<G>

  it('string global key is excluded from RebindGlobals entirely', () => {
    // `API_URL` must NOT appear as a key in Rebind — enforced at type level.
    // The @ts-expect-error below proves the property does not exist.
    // If this line compiles WITHOUT error, the test is broken.
    // @ts-expect-error — API_URL is a string global and must not be rebindable
    const _: Rebind = { API_URL: 'https://new' }
    expect(true).toBe(true) // runtime assertion is trivial; the type error above is the test
  })

  it('data global key is excluded from RebindGlobals entirely', () => {
    // @ts-expect-error — config is a data (constant) global and must not be rebindable
    const _: Rebind = { config: { kind: 'data', value: 1 } }
    expect(true).toBe(true)
  })

  it('shimmed global can only be rebound with the same handler type', () => {
    // Valid: same SafeFetchFn type
    const validOverride: Rebind = {
      fetch: (async () => ({ status: 200, body: null })) as SafeFetchFn,
    }
    expect(validOverride).toBeDefined()

    // Invalid: providing a new BridgeWithShim instead of just the handler function
    // @ts-expect-error — BridgeWithShim is not the rebind type; only the handler H is
    const _invalid: Rebind = {
      fetch: { kind: 'bridge-with-shim', handler: async () => ({}), shim: '' },
    }
    expect(true).toBe(true)
  })

  it('plain function global can be rebound with any HostExportFunction', () => {
    const override: Rebind = {
      myTool: () => 'new value',
    }
    expect(override).toBeDefined()
  })

  it('all rebind fields are optional — empty object is always valid', () => {
    const empty: Rebind = {}
    expect(empty).toBeDefined()
  })
})

// ─────────────────────────────────────────────────────────────────────────
// Bad paths — runtime behaviour
// ─────────────────────────────────────────────────────────────────────────

describe('bad paths — runtime', () => {
  describe('processGlobals edge cases', () => {
    it('unknown kind object is silently ignored (not a function, string, data, or shim)', () => {
      const { defs, dispatch } = processGlobals({
        weird: { kind: 'not-a-shim' } as unknown as HostExportFunction,
      })
      expect(defFor(defs, 'weird')).toBeUndefined()
      expect(Object.keys(dispatch)).toHaveLength(0)
    })

    it('empty globals map returns no defs and no dispatch entries', () => {
      const { defs, dispatch } = processGlobals({})
      expect(defs).toHaveLength(0)
      expect(Object.keys(dispatch)).toHaveLength(0)
    })
  })

  describe('extractBridgeGlobals — silently ignores invalid overrides', () => {
    it('string global override at run time is silently dropped (not a bridge function)', () => {
      // At runtime someone bypasses TypeScript with `as any`.
      // extractBridgeGlobals skips string entries entirely — no throw, no bridge global.
      const shimGlobal: BridgeWithShim = {
        kind: 'bridge-with-shim',
        handler: () => {},
        shim: '(r) => r',
      }
      const { dispatch } = extractBridgeGlobals(
        { fetch: 'https://evil.com' } as unknown as RebindGlobals<HostGlobals>,
        { fetch: shimGlobal },
      )
      // String override is ignored; falls back to default handler at the private key
      expect(dispatch['__iso4_fetch_h']).toBe(shimGlobal.handler)
    })

    it('null/undefined override falls back to precompile default', () => {
      const defaultFn: HostExportFunction = () => 'default'
      const { dispatch } = extractBridgeGlobals(
        { myTool: undefined as unknown as HostExportFunction },
        { myTool: defaultFn },
      )
      expect(dispatch['myTool']).toBe(defaultFn)
    })
  })
})

// ─────────────────────────────────────────────────────────────────────────
// Bad paths — type level (compile-time enforcement)
// ─────────────────────────────────────────────────────────────────────────

describe('bad paths — TypeScript type errors (compile-time only)', () => {
  type SafeFetchFn = (...args: unknown[]) => Promise<{ status: number, body: string }>
  interface G {
    fetch: BridgeWithShim<SafeFetchFn>
    myTool: HostExportFunction
    API_URL: string
  }
  type Rebind = RebindGlobals<G>

  it('string global key is excluded — @ts-expect-error proves it', () => {
    // TypeScript makes `API_URL` absent from Rebind; assigning it is an error.
    // @ts-expect-error — API_URL is a string global: not a valid rebind key
    const _1: Rebind = { API_URL: 'new-url' }

    // Even setting it to undefined is rejected (key doesn't exist)
    // @ts-expect-error — API_URL must not appear in RebindGlobals
    const _2: Rebind = { API_URL: undefined }

    expect(true).toBe(true) // runtime: both lines above must carry @ts-expect-error
  })

  it('BridgeWithShim rebind must be the handler function, not a new BridgeWithShim', () => {
    // Valid: handler function of the right type
    const valid: Rebind = {
      fetch: (async () => ({ status: 200, body: '{}' })) as SafeFetchFn,
    }
    expect(valid).toBeDefined()

    // Invalid: providing a BridgeWithShim where only the handler H is accepted
    // @ts-expect-error — rebind for a shimmed global must be H (SafeFetchFn), not BridgeWithShim
    const _invalid: Rebind = {
      fetch: { kind: 'bridge-with-shim', handler: async () => ({ status: 200, body: '{}' }), shim: '' },
    }
    expect(true).toBe(true)
  })

  it('plain function global rebind enforces the same function type', () => {
    // Valid: HostExportFunction
    const valid: Rebind = { myTool: () => 'result' }
    expect(valid).toBeDefined()

    // Invalid: number is not a function
    // @ts-expect-error — myTool must be HostExportFunction, not a number
    const _invalid: Rebind = { myTool: 42 }
    expect(true).toBe(true)
  })

  it('empty rebind object is always valid — all fields optional', () => {
    const empty: Rebind = {}
    expect(empty).toBeDefined()
  })
})
