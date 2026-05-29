/* eslint-disable no-restricted-properties */
/* eslint-disable no-proto */
import { describe, expect, test } from 'vitest'
import {
  WireDecodeError,
  decodeRunCompletionPayload,
  decodeWireValue,
  decodePrecompileResultPayload,
  encodeWireValue as encodeWireValueProduction,
} from '../src/wire.js'

// ── decodePrecompileResultPayload ──────────────────────────────────────────

// ── Test-only encoder ──────────────────────────────────────────────────────
//
// Mirrors the Rust `encode_wire_value` / `encode_run_completion_payload`
// functions so we can construct known-good byte fixtures for decoder tests.

function encodeWireValue(value: unknown): Uint8Array {
  const parts: number[] = []
  encodeValue(value, parts)
  return Uint8Array.from(parts)
}

function encodeValue(value: unknown, out: number[]): void {
  if (value === undefined) {
    out.push(0x00)
    return
  }
  if (value === null) {
    out.push(0x01)
    return
  }
  if (value === false) {
    out.push(0x02)
    return
  }
  if (value === true) {
    out.push(0x03)
    return
  }
  if (typeof value === 'number') {
    out.push(0x04)
    const buf = new ArrayBuffer(8)
    new DataView(buf).setFloat64(0, value, false)
    out.push(...new Uint8Array(buf))
    return
  }
  if (typeof value === 'string') {
    out.push(0x05)
    const bytes = new TextEncoder().encode(value)
    pushU32(out, bytes.length)
    out.push(...bytes)
    return
  }
  if (typeof value === 'bigint') {
    // New encoding: sign_bit (u8) + word_count (u32) + words (u64 BE, LSW first)
    const sign = value < 0n ? 1 : 0
    let magnitude = value < 0n ? -value : value
    const words: bigint[] = []
    while (magnitude > 0n) {
      words.push(magnitude & 0xffffffffffffffffn)
      magnitude >>= 64n
    }
    out.push(0x06)
    out.push(sign)
    pushU32(out, words.length)
    for (const word of words) pushU64BE(out, word)
    return
  }
  if (value instanceof Uint8Array) {
    out.push(0x07)
    pushU32(out, value.length)
    out.push(...value)
    return
  }
  if (Array.isArray(value)) {
    out.push(0x08)
    pushU32(out, value.length)
    for (const item of value as unknown[]) encodeValue(item, out)
    return
  }
  if (typeof value === 'object') {
    const entries = Object.entries(value as Record<string, unknown>)
    out.push(0x09)
    pushU32(out, entries.length)
    for (const [k, v] of entries) {
      const keyBytes = new TextEncoder().encode(k)
      pushU32(out, keyBytes.length)
      out.push(...keyBytes)
      encodeValue(v, out)
    }
    return
  }
  throw new Error(`test encoder: cannot encode value of type ${typeof value}`)
}

function pushU32(out: number[], n: number): void {
  out.push((n >>> 24) & 0xff, (n >>> 16) & 0xff, (n >>> 8) & 0xff, n & 0xff)
}

function pushU64BE(out: number[], n: bigint): void {
  pushU32(out, Number((n >> 32n) & 0xffffffffn))
  pushU32(out, Number(n & 0xffffffffn))
}

function pushF64(out: number[], n: number): void {
  const buf = new ArrayBuffer(8)
  new DataView(buf).setFloat64(0, n, false)
  out.push(...new Uint8Array(buf))
}

function encodeString(s: string, out: number[]): void {
  const bytes = new TextEncoder().encode(s)
  pushU32(out, bytes.length)
  out.push(...bytes)
}

function encodeStringList(items: string[], out: number[]): void {
  pushU32(out, items.length)
  for (const s of items) encodeString(s, out)
}

interface SuccessCompletion {
  ok: true
  exports: unknown
  stdout: string[]
  stderr: string[]
  durationMs: number
}

interface FailureCompletion {
  ok: false
  code: string
  name: string
  message: string
  stack?: string
  stdout: string[]
  stderr: string[]
  durationMs: number
}

function encodeCompletionPayload(
  runId: number,
  completion: SuccessCompletion | FailureCompletion,
): Uint8Array {
  const parts: number[] = []
  pushU32(parts, runId)

  if (completion.ok) {
    parts.push(1) // ok = true
    parts.push(1) // successPresent = 1
    encodeValue(completion.exports, parts)
    encodeStringList(completion.stdout, parts)
    encodeStringList(completion.stderr, parts)
    pushF64(parts, completion.durationMs)
    parts.push(0) // failurePresent = 0
  } else {
    parts.push(0) // ok = false
    parts.push(0) // successPresent = 0
    parts.push(1) // failurePresent = 1
    encodeString(completion.code, parts)
    encodeString(completion.name, parts)
    encodeString(completion.message, parts)
    if (completion.stack !== undefined) {
      parts.push(1)
      encodeString(completion.stack, parts)
    } else {
      parts.push(0)
    }
    encodeStringList(completion.stdout, parts)
    encodeStringList(completion.stderr, parts)
    pushF64(parts, completion.durationMs)
  }

  return Uint8Array.from(parts)
}

// ── decodeWireValue — primitives ───────────────────────────────────────────

describe('decodeWireValue — primitives', () => {
  test('undefined', () => {
    expect(decodeWireValue(encodeWireValue(undefined))).toBeUndefined()
  })

  test('null', () => {
    expect(decodeWireValue(encodeWireValue(null))).toBeNull()
  })

  test('false', () => {
    expect(decodeWireValue(encodeWireValue(false))).toBe(false)
  })

  test('true', () => {
    expect(decodeWireValue(encodeWireValue(true))).toBe(true)
  })

  test('number — integer', () => {
    expect(decodeWireValue(encodeWireValue(42))).toBe(42)
  })

  test('number — float', () => {
    expect(decodeWireValue(encodeWireValue(3.14))).toBeCloseTo(3.14)
  })

  test('number — negative', () => {
    expect(decodeWireValue(encodeWireValue(-1))).toBe(-1)
  })

  test('number — zero', () => {
    expect(decodeWireValue(encodeWireValue(0))).toBe(0)
  })

  test('number — 123.0 matches spec byte layout', () => {
    // docs/protocol.md §4.3: 123.0 = 0x405EC00000000000 big-endian
    const bytes = Uint8Array.from([
      0x04,
      0x40,
      0x5e,
      0xc0,
      0x00,
      0x00,
      0x00,
      0x00,
      0x00,
    ])
    expect(decodeWireValue(bytes)).toBe(123)
  })

  test('string', () => {
    expect(decodeWireValue(encodeWireValue('hello'))).toBe('hello')
  })

  test('empty string', () => {
    expect(decodeWireValue(encodeWireValue(''))).toBe('')
  })

  test('string with unicode', () => {
    expect(decodeWireValue(encodeWireValue('héllo 🌍'))).toBe('héllo 🌍')
  })

  test('bigint positive', () => {
    expect(decodeWireValue(encodeWireValue(42n))).toBe(42n)
  })

  test('bigint negative', () => {
    expect(decodeWireValue(encodeWireValue(-100n))).toBe(-100n)
  })

  test('bigint zero', () => {
    expect(decodeWireValue(encodeWireValue(0n))).toBe(0n)
  })

  test('bigint larger than u64 max roundtrips', () => {
    const v = 2n ** 64n
    expect(decodeWireValue(encodeWireValue(v))).toBe(v)
  })

  test('bigint negative larger than i64 min roundtrips', () => {
    const v = -(2n ** 65n)
    expect(decodeWireValue(encodeWireValue(v))).toBe(v)
  })

  test('bigint 2^128 roundtrips', () => {
    const v = 2n ** 128n
    expect(decodeWireValue(encodeWireValue(v))).toBe(v)
  })

  test('bigint production encoder matches test encoder', () => {
    // Ensures the production WireWriter and the test encodeValue helper
    // produce identical bytes for a range of BigInt values.
    // Compare as Uint8Array — production returns Buffer (a Buffer subclass),
    // test helper returns Uint8Array; normalise to avoid type-equality failure.
    for (const v of [0n, 1n, -1n, 42n, -100n, 2n ** 64n, -(2n ** 65n), 2n ** 128n]) {
      const prod = new Uint8Array(encodeWireValueProduction(v))
      const test = encodeWireValue(v)
      expect(prod).toEqual(test)
    }
  })

  test('bytes', () => {
    const input = Uint8Array.from([0xde, 0xad, 0xbe, 0xef])
    const result = decodeWireValue(encodeWireValue(input))
    expect(result).toBeInstanceOf(Uint8Array)
    expect(result).toEqual(input)
  })

  test('empty bytes', () => {
    const result = decodeWireValue(encodeWireValue(new Uint8Array(0)))
    expect(result).toBeInstanceOf(Uint8Array)
    expect((result as Uint8Array).length).toBe(0)
  })
})

// ── decodeWireValue — collections ──────────────────────────────────────────

describe('decodeWireValue — collections', () => {
  test('empty array', () => {
    expect(decodeWireValue(encodeWireValue([]))).toEqual([])
  })

  test('array of primitives', () => {
    expect(decodeWireValue(encodeWireValue([1, 'two', true, null]))).toEqual([
      1,
      'two',
      true,
      null,
    ])
  })

  test('empty object', () => {
    expect(decodeWireValue(encodeWireValue({}))).toEqual({})
  })

  test('flat object', () => {
    expect(decodeWireValue(encodeWireValue({ x: 1, y: 2 }))).toEqual({
      x: 1,
      y: 2,
    })
  })

  test('nested object', () => {
    const value = { outer: { inner: 42 } }
    expect(decodeWireValue(encodeWireValue(value))).toEqual(value)
  })

  test('object with array value', () => {
    const value = { items: [1, 2, 3] }
    expect(decodeWireValue(encodeWireValue(value))).toEqual(value)
  })

  test('array of objects', () => {
    const value = [{ a: 1 }, { b: 2 }]
    expect(decodeWireValue(encodeWireValue(value))).toEqual(value)
  })

  test('deeply nested — object in array in object in array', () => {
    const value = { a: [{ b: [{ c: 42 }, { d: 'hello' }] }] }
    expect(decodeWireValue(encodeWireValue(value))).toEqual(value)
  })
})

// ── decodeWireValue — protocol example from docs/protocol.md §4.3 ─────────

describe('decodeWireValue — protocol spec example', () => {
  // export const someExport = { hello: ['some', 123] }
  //
  // Byte layout documented in docs/protocol.md §4.3 example.
  const SPEC_BYTES = Uint8Array.from([
    0x09, // Object
    0x00,
    0x00,
    0x00,
    0x01, // 1 field
    0x00,
    0x00,
    0x00,
    0x0a, // key len = 10
    0x73,
    0x6f,
    0x6d,
    0x65,
    0x45,
    0x78,
    0x70,
    0x6f,
    0x72,
    0x74, // "someExport"
    0x09, // Object
    0x00,
    0x00,
    0x00,
    0x01, // 1 field
    0x00,
    0x00,
    0x00,
    0x05, // key len = 5
    0x68,
    0x65,
    0x6c,
    0x6c,
    0x6f, // "hello"
    0x08, // Array
    0x00,
    0x00,
    0x00,
    0x02, // 2 items
    0x05, // String
    0x00,
    0x00,
    0x00,
    0x04, // len = 4
    0x73,
    0x6f,
    0x6d,
    0x65, // "some"
    0x04, // Number
    0x40,
    0x5e,
    0xc0,
    0x00,
    0x00,
    0x00,
    0x00,
    0x00, // 123.0
  ])

  test('decodes spec bytes to correct JS value', () => {
    expect(decodeWireValue(SPEC_BYTES)).toEqual({
      someExport: { hello: ['some', 123] },
    })
  })

  test('encoder produces the same bytes as the spec', () => {
    const encoded = encodeWireValue({ someExport: { hello: ['some', 123] } })
    expect(encoded).toEqual(SPEC_BYTES)
  })

  test('round-trip via encoder matches spec bytes', () => {
    const decoded = decodeWireValue(SPEC_BYTES)
    const reencoded = encodeWireValue(decoded)
    expect(reencoded).toEqual(SPEC_BYTES)
  })

  test('default + named exports flat object (second spec example)', () => {
    // export default { ok: true }
    // export const count = 2
    const value = { default: { ok: true }, count: 2 }
    expect(decodeWireValue(encodeWireValue(value))).toEqual(value)
  })
})

// ── decodeWireValue — error cases ──────────────────────────────────────────

describe('decodeWireValue — errors', () => {
  test('unknown tag', () => {
    expect(() => decodeWireValue(Uint8Array.from([0xff]))).toThrow(WireDecodeError)
    expect(() => decodeWireValue(Uint8Array.from([0xff]))).toThrow('0xff')
  })

  test('empty buffer', () => {
    expect(() => decodeWireValue(new Uint8Array(0))).toThrow(WireDecodeError)
  })

  test('truncated number — missing f64 bytes', () => {
    // TAG_NUMBER but only 4 bytes of f64 instead of 8
    expect(() =>
      decodeWireValue(Uint8Array.from([0x04, 0x00, 0x00, 0x00, 0x00])),
    ).toThrow(WireDecodeError)
  })

  test('truncated string — body shorter than declared length', () => {
    // TAG_STRING, len=10, but only 3 bytes follow
    expect(() =>
      decodeWireValue(
        Uint8Array.from([0x05, 0x00, 0x00, 0x00, 0x0a, 0x61, 0x62, 0x63]),
      ),
    ).toThrow(WireDecodeError)
  })

  test('truncated array — fewer items than declared count', () => {
    // TAG_ARRAY, count=3, but only 1 item (TAG_NULL)
    expect(() =>
      decodeWireValue(Uint8Array.from([0x08, 0x00, 0x00, 0x00, 0x03, 0x01])),
    ).toThrow(WireDecodeError)
  })

  test('trailing bytes after value', () => {
    // TAG_NULL followed by an extra byte
    expect(() =>
      decodeWireValue(Uint8Array.from([0x01, 0x00])),
    ).toThrow(WireDecodeError)
  })
})

// ── decodeRunCompletionPayload — success ───────────────────────────────────

describe('decodeRunCompletionPayload — success', () => {
  test('basic success with default export', () => {
    const buf = encodeCompletionPayload(0, {
      ok: true,
      exports: { default: 42 },
      stdout: [],
      stderr: [],
      durationMs: 1.5,
    })
    const { runId, result } = decodeRunCompletionPayload(buf)
    expect(runId).toBe(0)
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports['default']).toBe(42)
    expect(result.stdout).toEqual([])
    expect(result.stderr).toEqual([])
    expect(result.durationMs).toBeCloseTo(1.5)
  })

  test('named exports decoded correctly', () => {
    const buf = encodeCompletionPayload(0, {
      ok: true,
      exports: { default: 'hello', count: 3, flag: true },
      stdout: [],
      stderr: [],
      durationMs: 0,
    })
    const { result } = decodeRunCompletionPayload(buf)
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports['default']).toBe('hello')
    expect(result.exports['count']).toBe(3)
    expect(result.exports['flag']).toBe(true)
  })

  test('nested object export', () => {
    const buf = encodeCompletionPayload(0, {
      ok: true,
      exports: { default: { x: 1, y: [2, 3] } },
      stdout: [],
      stderr: [],
      durationMs: 0,
    })
    const { result } = decodeRunCompletionPayload(buf)
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports['default']).toEqual({ x: 1, y: [2, 3] })
  })

  test('stdout and stderr lines captured', () => {
    const buf = encodeCompletionPayload(0, {
      ok: true,
      exports: { default: null },
      stdout: ['line one', 'line two'],
      stderr: ['warn: something'],
      durationMs: 5,
    })
    const { result } = decodeRunCompletionPayload(buf)
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.stdout).toEqual(['line one', 'line two'])
    expect(result.stderr).toEqual(['warn: something'])
  })

  test('durationMs is preserved exactly', () => {
    const buf = encodeCompletionPayload(0, {
      ok: true,
      exports: {},
      stdout: [],
      stderr: [],
      durationMs: 123.456,
    })
    const { result } = decodeRunCompletionPayload(buf)
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.durationMs).toBeCloseTo(123.456)
  })

  test('runId is echoed correctly', () => {
    const buf = encodeCompletionPayload(7, {
      ok: true,
      exports: {},
      stdout: [],
      stderr: [],
      durationMs: 0,
    })
    const { runId } = decodeRunCompletionPayload(buf)
    expect(runId).toBe(7)
  })

  test('empty exports object', () => {
    const buf = encodeCompletionPayload(0, {
      ok: true,
      exports: {},
      stdout: [],
      stderr: [],
      durationMs: 0,
    })
    const { result } = decodeRunCompletionPayload(buf)
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports).toEqual({})
  })
})

// ── decodeRunCompletionPayload — failure ───────────────────────────────────

describe('decodeRunCompletionPayload — failure', () => {
  test('compile error with no stack', () => {
    const buf = encodeCompletionPayload(0, {
      ok: false,
      code: 'ERR_COMPILE',
      name: 'SyntaxError',
      message: 'unexpected token',
      stdout: [],
      stderr: [],
      durationMs: 0,
    })
    const { result } = decodeRunCompletionPayload(buf)
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.code).toBe('ERR_COMPILE')
    expect(result.error.name).toBe('SyntaxError')
    expect(result.error.message).toBe('unexpected token')
    expect(result.error.stack).toBeUndefined()
  })

  test('runtime error with stack trace', () => {
    const buf = encodeCompletionPayload(0, {
      ok: false,
      code: 'ERR_USER_CODE',
      name: 'Error',
      message: 'boom',
      stack: 'Error: boom\n  at <iso4>:1:1',
      stdout: ['before throw'],
      stderr: [],
      durationMs: 2,
    })
    const { result } = decodeRunCompletionPayload(buf)
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.code).toBe('ERR_USER_CODE')
    expect(result.error.message).toBe('boom')
    expect(result.error.stack).toContain('at <iso4>')
    expect(result.stdout).toEqual(['before throw'])
  })

  test('logs are preserved on failure', () => {
    const buf = encodeCompletionPayload(0, {
      ok: false,
      code: 'ERR_USER_CODE',
      name: 'Error',
      message: 'fail',
      stdout: ['stdout line'],
      stderr: ['stderr line'],
      durationMs: 1,
    })
    const { result } = decodeRunCompletionPayload(buf)
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.stdout).toEqual(['stdout line'])
    expect(result.stderr).toEqual(['stderr line'])
  })

  test('all error codes round-trip', () => {
    const codes = [
      'ERR_USER_CODE',
      'ERR_COMPILE',
      'ERR_MODULE_NOT_FOUND',
      'ERR_EXPORT_NOT_SERIALIZABLE',
      'ERR_CPU_TIMEOUT',
      'ERR_WALL_TIMEOUT',
      'ERR_MEMORY_LIMIT',
      'ERR_INTERNAL',
    ] as const

    for (const code of codes) {
      const buf = encodeCompletionPayload(0, {
        ok: false,
        code,
        name: 'Error',
        message: 'msg',
        stdout: [],
        stderr: [],
        durationMs: 0,
      })
      const { result } = decodeRunCompletionPayload(buf)
      expect(result.ok).toBe(false)
      if (result.ok)
        continue
      expect(result.error.code).toBe(code)
    }
  })

  test('durationMs is preserved on failure', () => {
    const buf = encodeCompletionPayload(0, {
      ok: false,
      code: 'ERR_USER_CODE',
      name: 'Error',
      message: 'x',
      stdout: [],
      stderr: [],
      durationMs: 42.5,
    })
    const { result } = decodeRunCompletionPayload(buf)
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.durationMs).toBeCloseTo(42.5)
  })
})

// ── decodeRunCompletionPayload — error cases ───────────────────────────────

describe('decodeRunCompletionPayload — errors', () => {
  test('empty buffer', () => {
    expect(() => decodeRunCompletionPayload(new Uint8Array(0))).toThrow(
      WireDecodeError,
    )
  })

  test('truncated after runId', () => {
    expect(() =>
      decodeRunCompletionPayload(Uint8Array.from([0x00, 0x00, 0x00, 0x00])),
    ).toThrow(WireDecodeError)
  })

  test('invalid bool byte in ok field', () => {
    expect(() =>
      decodeRunCompletionPayload(
        Uint8Array.from([0x00, 0x00, 0x00, 0x00, 0x02]),
      ),
    ).toThrow(WireDecodeError)
  })
})

describe('decodePrecompileResultPayload', () => {
  function encodePrecompileSuccess(prefixId: string): Uint8Array {
    const parts: number[] = []
    parts.push(1) // ok = true
    parts.push(1) // prefixIdPresent = 1
    encodeString(prefixId, parts)
    parts.push(0) // errorPresent = 0
    return Uint8Array.from(parts)
  }

  function encodePrecompileFailure(code: string, message: string, stack?: string): Uint8Array {
    const parts: number[] = []
    parts.push(0) // ok = false
    parts.push(0) // prefixIdPresent = 0
    parts.push(1) // errorPresent = 1
    encodeString(code, parts)
    encodeString('SyntaxError', parts)
    encodeString(message, parts)
    if (stack !== undefined) {
      parts.push(1)
      encodeString(stack, parts)
    } else {
      parts.push(0)
    }
    return Uint8Array.from(parts)
  }

  test('success decodes prefixId', () => {
    const result = decodePrecompileResultPayload(encodePrecompileSuccess('42'))
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.prefixId).toBe('42')
  })

  test('failure decodes error fields', () => {
    const result = decodePrecompileResultPayload(
      encodePrecompileFailure('ERR_COMPILE', 'bad syntax', 'at line 1'),
    )
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.code).toBe('ERR_COMPILE')
    expect(result.error.message).toBe('bad syntax')
    expect(result.error.stack).toBe('at line 1')
  })

  test('failure without stack', () => {
    const result = decodePrecompileResultPayload(
      encodePrecompileFailure('ERR_USER_CODE', 'boom'),
    )
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.stack).toBeUndefined()
  })
})

// ── __proto__ elision — encoder (host → Rust direction) ───────────────────
//
// `Object.entries` on a null-proto object (the kind decodeWireValue produces)
// includes any own "__proto__" data property.  The production encoder must
// filter it so a decoded sandbox value echoed back as a bridge response does
// not forward __proto__ to the sandbox.

describe('encodeWireValue — __proto__ elision', () => {
  test('__proto__ own key on null-proto object is dropped', () => {
    // Object.create(null) + obj['__proto__'] = x is the round-trip path:
    // sandbox → TS decodes (null-proto obj) → host returns it → encoder
    const obj = Object.create(null) as Record<string, unknown>
    obj['x'] = 1
    obj['__proto__'] = { polluted: true }
    obj['y'] = 2

    const decoded = decodeWireValue(encodeWireValueProduction(obj)) as Record<string, unknown>
    expect(decoded['x']).toBe(1)
    expect(decoded['y']).toBe(2)
    expect(Object.hasOwn(decoded, '__proto__')).toBe(false)
  })

  test('keys before and after __proto__ are all preserved', () => {
    const obj = Object.create(null) as Record<string, unknown>
    obj['before'] = 'a'
    obj['__proto__'] = 'evil'
    obj['after'] = 'b'

    const decoded = decodeWireValue(encodeWireValueProduction(obj)) as Record<string, unknown>
    expect(decoded['before']).toBe('a')
    expect(decoded['after']).toBe('b')
    expect(Object.hasOwn(decoded, '__proto__')).toBe(false)
  })

  test('plain object literal never has own __proto__ — unaffected', () => {
    // { __proto__: val } sets the prototype, not an own property, so
    // Object.entries won't include it regardless. The filter is a no-op here.
    const decoded = decodeWireValue(encodeWireValueProduction({ a: 1 })) as Record<string, unknown>
    expect(decoded['a']).toBe(1)
    expect(Object.hasOwn(decoded, '__proto__')).toBe(false)
  })
})

// ── __proto__ elision — decoder (Rust → TS direction, defence-in-depth) ───
//
// Rust's serialize_object_fields already drops "__proto__" before encoding,
// so the TS decoder should never see it in practice.  The guard is there as
// a self-contained safety net.  These tests use the local test encoder (no
// filtering) to craft wire bytes that contain a __proto__ key directly.

describe('decodeWireValue — __proto__ elision (defence-in-depth)', () => {
  test('__proto__ key in wire bytes is dropped, sibling keys survive', () => {
    // Use the local test encoder (no __proto__ filter) to craft the bytes.
    const wire = encodeWireValue(
      (() => {
        const o = Object.create(null) as Record<string, unknown>
        o['x'] = 1
        o['__proto__'] = 99
        o['y'] = 2
        return o
      })(),
    )

    const decoded = decodeWireValue(wire) as Record<string, unknown>
    expect(decoded['x']).toBe(1)
    expect(decoded['y']).toBe(2)
    expect(Object.hasOwn(decoded, '__proto__')).toBe(false)
    // Prototype must be null — Object.create(null)
    expect(Object.getPrototypeOf(decoded)).toBeNull()
  })

  test('value bytes after a dropped __proto__ are consumed — no reader corruption', () => {
    // A complex nested value under __proto__ must be fully consumed so
    // subsequent keys decode correctly.
    const wire = encodeWireValue(
      (() => {
        const o = Object.create(null) as Record<string, unknown>
        o['__proto__'] = { deeply: { nested: [1, 2, 3] } }
        o['after'] = 'clean'
        return o
      })(),
    )

    const decoded = decodeWireValue(wire) as Record<string, unknown>
    expect(decoded['after']).toBe('clean')
    expect(Object.hasOwn(decoded, '__proto__')).toBe(false)
  })
})

// ── encodeWireValue — cycle detection ─────────────────────────────────────

describe('encodeWireValue — cycle detection', () => {
  test('object self-reference throws TypeError', () => {
    const a: Record<string, unknown> = {}
    a['self'] = a
    expect(() => encodeWireValueProduction(a)).toThrow(TypeError)
    expect(() => encodeWireValueProduction(a)).toThrow('cyclic')
  })

  test('array self-reference throws TypeError', () => {
    const a: unknown[] = []
    a.push(a)
    expect(() => encodeWireValueProduction(a)).toThrow(TypeError)
    expect(() => encodeWireValueProduction(a)).toThrow('cyclic')
  })

  test('mutual object cycle throws TypeError', () => {
    const a: Record<string, unknown> = {}
    const b: Record<string, unknown> = { a }
    a['b'] = b
    expect(() => encodeWireValueProduction(a)).toThrow(TypeError)
  })

  test('array containing itself throws TypeError', () => {
    const a: unknown[] = [1, 2]
    a.push(a)
    expect(() => encodeWireValueProduction(a)).toThrow(TypeError)
  })

  test('diamond shape (same object at two leaves) does NOT throw', () => {
    // Diamonds are not cycles — the shared node appears twice in the
    // tree but is never an ancestor of itself on the current path.
    const shared = { v: 42 }
    expect(() => encodeWireValueProduction({ a: shared, b: shared })).not.toThrow()
  })

  test('diamond array (same array at two leaves) does NOT throw', () => {
    const shared = [1, 2, 3]
    expect(() => encodeWireValueProduction([shared, shared])).not.toThrow()
  })

  test('deep mutual cycle throws TypeError', () => {
    const a: Record<string, unknown> = { x: { y: {} } }
    ;((a['x'] as Record<string, unknown>)['y'] as Record<string, unknown>)['back'] = a
    expect(() => encodeWireValueProduction(a)).toThrow(TypeError)
  })
})
