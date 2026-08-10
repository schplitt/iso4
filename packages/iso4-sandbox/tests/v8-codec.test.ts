/* eslint-disable no-restricted-properties */
/* eslint-disable no-proto */
/**
 * Host-side value codec — `serializeValue` / `deserializeValue`.
 *
 * These are the bytes that cross the socket in both directions, so what this
 * file asserts is the value contract of the protocol (`docs/protocol.md` §4).
 * The Rust mirror of this suite lives in `native/v8-runtime/src/blob.rs`.
 */

import { Buffer } from 'node:buffer'
import { describe, expect, test } from 'vitest'
import {
  ValueDecodeError,
  ValueEncodeError,
  deserializeValue,
  probeFormatVersion,
  serializationProbe,
  serializeValue,
} from '../src/v8-codec.js'

function roundtrip(value: unknown): unknown {
  return deserializeValue(serializeValue(value))
}

describe('serializeValue / deserializeValue — primitives', () => {
  test.each([
    ['undefined', undefined],
    ['null', null],
    ['false', false],
    ['true', true],
    ['integer', 42],
    ['float', 3.14],
    ['negative', -17.5],
    ['negative zero', -0],
    ['NaN', Number.NaN],
    ['Infinity', Number.POSITIVE_INFINITY],
    ['string', 'hello'],
    ['empty string', ''],
    ['unicode string', 'héllo 🌍 世界'],
  ])('%s round-trips', (_name, value) => {
    expect(roundtrip(value)).toEqual(value)
  })

  test.each([
    ['positive', 42n],
    ['negative', -100n],
    ['zero', 0n],
    ['above u64 max', 2n ** 64n + 7n],
    ['below i64 min', -(2n ** 65n)],
    ['2^128', 2n ** 128n],
  ])('bigint %s round-trips', (_name, value) => {
    expect(roundtrip(value)).toBe(value)
  })
})

describe('serializeValue / deserializeValue — collections', () => {
  test('empty array', () => {
    expect(roundtrip([])).toEqual([])
  })

  test('array of mixed primitives', () => {
    expect(roundtrip([1, 'two', true, null, undefined])).toEqual([1, 'two', true, null, undefined])
  })

  test('sparse array keeps its holes', () => {
    const sparse: unknown[] = [1]
    sparse[50] = 2
    const back = roundtrip(sparse) as unknown[]
    expect(back.length).toBe(51)
    expect(back[0]).toBe(1)
    expect(back[50]).toBe(2)
    expect(Object.hasOwn(back, 25)).toBe(false)
  })

  test('empty object', () => {
    expect(roundtrip({})).toEqual({})
  })

  test('deeply nested object/array alternation', () => {
    const value = { a: [{ b: [{ c: [1, 2, 3] }] }] }
    expect(roundtrip(value)).toEqual(value)
  })

  test('a shared reference decodes to one object, not two copies', () => {
    const shared = { x: 1 }
    const back = roundtrip({ a: shared, b: shared }) as { a: object, b: object }
    expect(back.a).toEqual({ x: 1 })
    expect(back.a).toBe(back.b)
  })

  test('a cycle round-trips', () => {
    const cyclic: Record<string, unknown> = { x: 1 }
    cyclic.self = cyclic
    const back = roundtrip(cyclic) as Record<string, unknown>
    expect(back.x).toBe(1)
    expect(back.self).toBe(back)
  })
})

// The V8 format carries these natively; the codec they replaced rejected them.
describe('serializeValue / deserializeValue — real instances', () => {
  test('Date', () => {
    const back = roundtrip(new Date(1700000000000))
    expect(back).toBeInstanceOf(Date)
    expect((back as Date).getTime()).toBe(1700000000000)
  })

  test('Map with non-string keys', () => {
    const back = roundtrip(new Map<unknown, unknown>([['a', 1], [2, 'b']]))
    expect(back).toBeInstanceOf(Map)
    expect([...(back as Map<unknown, unknown>)]).toEqual([['a', 1], [2, 'b']])
  })

  test('Set', () => {
    const back = roundtrip(new Set([1, 2, 3]))
    expect(back).toBeInstanceOf(Set)
    expect([...(back as Set<number>)]).toEqual([1, 2, 3])
  })

  test('RegExp keeps source and flags', () => {
    const back = roundtrip(/ab+c/gi) as RegExp
    expect(back).toBeInstanceOf(RegExp)
    expect(back.source).toBe('ab+c')
    expect(back.flags).toBe('gi')
  })

  test('Error keeps its constructor and message', () => {
    const back = roundtrip(new TypeError('boom')) as Error
    expect(back).toBeInstanceOf(TypeError)
    expect(back.message).toBe('boom')
  })

  test('ArrayBuffer', () => {
    const back = roundtrip(new Uint8Array([1, 2, 3]).buffer)
    expect(back).toBeInstanceOf(ArrayBuffer)
    expect([...new Uint8Array(back as ArrayBuffer)]).toEqual([1, 2, 3])
  })

  test.each([
    ['Uint8Array', new Uint8Array([1, 2, 3])],
    ['Float32Array', new Float32Array([1.5, -2.5])],
    ['Int32Array', new Int32Array([-1, 2 ** 30])],
    ['BigInt64Array', new BigInt64Array([-1n, 2n ** 40n])],
    ['empty Uint8Array', new Uint8Array(0)],
  ])('%s keeps its element type', (_name, view) => {
    const back = roundtrip(view)
    expect(back).toBeInstanceOf(view.constructor as never)
    expect([...(back as never as ArrayLike<unknown>)]).toEqual([...(view as never as ArrayLike<unknown>)])
  })

  test('a subarray window carries only its window', () => {
    const window = new Uint8Array([0, 1, 2, 3, 4]).subarray(1, 4)
    const back = roundtrip(window) as Uint8Array
    expect([...back]).toEqual([1, 2, 3])
    expect(back.byteLength).toBe(3)
  })
})

describe('serializeValue — loud failures', () => {
  test.each([
    ['a function', () => 1],
    ['a symbol', Symbol('s')],
    ['a WeakMap', new WeakMap()],
    ['a Promise', Promise.resolve(1)],
  ])('%s throws ValueEncodeError', (_name, value) => {
    expect(() => serializeValue(value)).toThrow(ValueEncodeError)
    expect(() => serializeValue(value)).toThrow(/cannot serialize value/)
  })

  test('a function nested inside a plain object also throws', () => {
    expect(() => serializeValue({ ok: 1, fn: () => 1 })).toThrow(ValueEncodeError)
  })
})

describe('deserializeValue — loud failures', () => {
  test.each([
    ['empty bytes', new Uint8Array()],
    ['garbage', new Uint8Array([0x00, 0x01, 0x02])],
    ['an impossible format version', new Uint8Array([0xFF, 0x63, 0x30])],
    ['a truncated blob', serializeValue({ a: 'longer string' }).subarray(0, 6)],
  ])('%s throws ValueDecodeError', (_name, bytes) => {
    expect(() => deserializeValue(bytes)).toThrow(ValueDecodeError)
  })
})

// Behaviour change vs the codec this replaced, which dropped the key in both
// directions. V8's serializer writes it as a plain own data property and the
// deserializer defines (never [[Set]]s) it, so no prototype is reachable.
describe('__proto__ as an own key', () => {
  test('survives as a plain own data property', () => {
    const source = Object.create(null) as Record<string, unknown>
    source.before = 1
    source['__proto__'] = { polluted: true }
    source.after = 2

    const back = roundtrip(source) as Record<string, unknown>
    expect(Object.hasOwn(back, '__proto__')).toBe(true)
    expect(back.before).toBe(1)
    expect(back.after).toBe(2)
  })

  test('does not pollute the receiving object or Object.prototype', () => {
    const source = Object.create(null) as Record<string, unknown>
    source['__proto__'] = { polluted: true }

    const back = roundtrip(source) as Record<string, unknown>
    // The decoded object is an ordinary plain object — the `__proto__` entry
    // landed as data, not as a prototype assignment.
    expect(Object.getPrototypeOf(back)).toBe(Object.prototype)
    expect(({} as Record<string, unknown>).polluted).toBeUndefined()
  })

  test('a plain object literal never has an own __proto__ to begin with', () => {
    // `{ __proto__: v }` sets the prototype rather than defining a key.
    const back = roundtrip({ __proto__: { polluted: true }, x: 1 }) as Record<string, unknown>
    expect(Object.hasOwn(back, '__proto__')).toBe(false)
    expect(back.x).toBe(1)
  })
})

// Accepted trade-off (docs/protocol.md §4.2): Node exposes no serializer hook
// to reject class instances, so they flatten to their own enumerable props.
describe('class instances', () => {
  test('flatten to their own enumerable properties', () => {
    class Row {
      value = 1
      label = 'x'
      describe(): string {
        return this.label
      }
    }
    const back = roundtrip(new Row()) as Record<string, unknown>
    expect(back).toEqual({ value: 1, label: 'x' })
    expect(back.describe).toBeUndefined()
    expect(back).not.toBeInstanceOf(Row)
  })
})

describe('serialization probe', () => {
  test('is a serialized null carrying this Node\'s format version', () => {
    const probe = serializationProbe()
    expect(probe[0]).toBe(0xFF)
    expect(deserializeValue(probe)).toBeNull()
    expect(probeFormatVersion(probe)).toBe(probe[1])
  })

  test('probeFormatVersion rejects bytes that are not a blob', () => {
    expect(probeFormatVersion(new Uint8Array())).toBeUndefined()
    expect(probeFormatVersion(Buffer.from([0x01, 0x02]))).toBeUndefined()
  })
})
