import { describe, expect, test } from 'vitest'
import { WireDecodeError, decodeRunCompletionPayload, decodeWireValue } from '../src/wire'

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
    out.push(0x06)
    const bytes = new TextEncoder().encode(value.toString())
    pushU32(out, bytes.length)
    out.push(...bytes)
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
