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
  HostExportFunction,
  HostGlobals,
  RebindGlobals,
} from '../src/types.js'
import { extractBridgeGlobals, processGlobals } from '../src/globals.js'

// ─────────────────────────────────────────────────────────────────────────
// processGlobals — preamble + bridge global extraction
// ─────────────────────────────────────────────────────────────────────────

describe('processGlobals', () => {
  describe('HostExportFunction', () => {
    it('registers function under its own name, no preamble', () => {
      const fn: HostExportFunction = () => 42
      const { bridgeGlobals, preamble } = processGlobals({ myTool: fn })
      expect(bridgeGlobals['myTool']).toBe(fn)
      expect(preamble).toBeUndefined()
    })

    it('registers multiple plain functions independently', () => {
      const a: HostExportFunction = () => 'a'
      const b: HostExportFunction = () => 'b'
      const { bridgeGlobals, preamble } = processGlobals({ a, b })
      expect(bridgeGlobals['a']).toBe(a)
      expect(bridgeGlobals['b']).toBe(b)
      expect(preamble).toBeUndefined()
    })
  })

  describe('string global', () => {
    it('generates preamble, no bridge global', () => {
      const { bridgeGlobals, preamble } = processGlobals({
        PI: `Math.PI`,
      })
      expect(Object.keys(bridgeGlobals)).toHaveLength(0)
      expect(preamble).toBe('globalThis["PI"] = (Math.PI)')
    })

    it('wraps the expression in globalThis assignment', () => {
      const { preamble } = processGlobals({ VERSION: `'2.0'` })
      expect(preamble).toBe('globalThis["VERSION"] = (\'2.0\')')
    })

    it('multiple string globals produce multiple preamble lines', () => {
      const { preamble } = processGlobals({ A: `1`, B: `2` })
      expect(preamble?.split('\n')).toHaveLength(2)
      expect(preamble).toContain('globalThis["A"] = (1)')
      expect(preamble).toContain('globalThis["B"] = (2)')
    })
  })

  describe('BridgeWithShim', () => {
    const handler: HostExportFunction = async () => ({ status: 200, body: null })
    const shim = `(result) => ({ ...result, ok: result.status < 300 })`

    const shimGlobal: BridgeWithShim = { kind: 'bridge-with-shim', handler, shim }

    it('registers handler under private __iso4_<name>_h key, not the public name', () => {
      const { bridgeGlobals } = processGlobals({ fetch: shimGlobal })
      expect(bridgeGlobals['fetch']).toBeUndefined()
      expect(bridgeGlobals['__iso4_fetch_h']).toBe(handler)
    })

    it('generates wrapper preamble that calls the private stub then the shim', () => {
      const { preamble } = processGlobals({ fetch: shimGlobal })
      expect(preamble).toContain('globalThis["fetch"]')
      expect(preamble).toContain('__iso4_fetch_h')
      expect(preamble).toContain(shim)
      // wrapper is async and awaits both the stub and the shim
      expect(preamble).toContain('async (...args)')
      expect(preamble).toContain('await')
    })

    it('uses the correct private key for any global name', () => {
      const { bridgeGlobals, preamble } = processGlobals({ myApi: shimGlobal })
      expect(bridgeGlobals['__iso4_myApi_h']).toBe(handler)
      expect(preamble).toContain('globalThis["myApi"]')
      expect(preamble).toContain('__iso4_myApi_h')
    })

    it('mixes plain functions, string globals, and shims correctly', () => {
      const plainFn: HostExportFunction = () => 'plain'
      const { bridgeGlobals, preamble } = processGlobals({
        fetch: shimGlobal, // BridgeWithShim
        myTool: plainFn, // HostExportFunction
        API_URL: `'https://api'`, // string
      })

      // Bridge globals: plain function under own name, shim under private key
      expect(bridgeGlobals['myTool']).toBe(plainFn)
      expect(bridgeGlobals['__iso4_fetch_h']).toBe(handler)
      expect(bridgeGlobals['fetch']).toBeUndefined()
      expect(bridgeGlobals['API_URL']).toBeUndefined()

      // Preamble: only string and shim generate preamble
      expect(preamble).toContain('globalThis["API_URL"]')
      expect(preamble).toContain('globalThis["fetch"]')
      expect(preamble).not.toContain('myTool') // plain fn has no preamble
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
  }

  it('string globals are skipped — they have no bridge function', () => {
    const out = extractBridgeGlobals({}, precompileGlobals)
    expect(Object.keys(out)).not.toContain('API_URL')
  })

  it('shimmed global with no override uses the precompile handler at private key', () => {
    const out = extractBridgeGlobals({}, precompileGlobals)
    expect(out['__iso4_fetch_h']).toBe(defaultHandler)
    expect(out['fetch']).toBeUndefined()
  })

  it('plain function with no override uses the precompile handler at own name', () => {
    const out = extractBridgeGlobals({}, precompileGlobals)
    expect(out['myTool']).toBe(plainFn)
  })

  it('shimmed global with function override: override goes to private key', () => {
    const newHandler: HostExportFunction = async () => ({ status: 418, body: 'teapot' })
    const out = extractBridgeGlobals({ fetch: newHandler }, precompileGlobals)
    expect(out['__iso4_fetch_h']).toBe(newHandler)
    expect(out['fetch']).toBeUndefined()
  })

  it('plain function with override: override goes to own name', () => {
    const newTool: HostExportFunction = () => 'overridden'
    const out = extractBridgeGlobals({ myTool: newTool }, precompileGlobals)
    expect(out['myTool']).toBe(newTool)
  })

  it('produces a flat map of only bridge functions — no preamble needed at run time', () => {
    const out = extractBridgeGlobals({}, precompileGlobals)
    // Every value must be a function
    for (const v of Object.values(out)) {
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
    it('unknown kind object is silently ignored (not a function, string, or BridgeWithShim)', () => {
      // An object that isn't a BridgeWithShim (wrong kind) should not crash.
      // Treated as a function (typeof check comes first) — not the intended use,
      // but shouldn't throw.
      expect(() => processGlobals({ weird: { kind: 'not-a-shim' } as unknown as HostExportFunction })).not.toThrow()
    })

    it('empty globals map returns no bridge globals and no preamble', () => {
      const { bridgeGlobals, preamble } = processGlobals({})
      expect(Object.keys(bridgeGlobals)).toHaveLength(0)
      expect(preamble).toBeUndefined()
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
      const out = extractBridgeGlobals(
        { fetch: 'https://evil.com' } as unknown as RebindGlobals<HostGlobals>,
        { fetch: shimGlobal },
      )
      // String override is ignored; falls back to default handler at the private key
      expect(out['__iso4_fetch_h']).toBe(shimGlobal.handler)
    })

    it('null/undefined override falls back to precompile default', () => {
      const defaultFn: HostExportFunction = () => 'default'
      const out = extractBridgeGlobals(
        { myTool: undefined as unknown as HostExportFunction },
        { myTool: defaultFn },
      )
      expect(out['myTool']).toBe(defaultFn)
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
