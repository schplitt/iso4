/**
 * Binary wire codec - decoder for the iso4 Rust → TypeScript protocol.
 *
 * Implements `WireValue` decoding per `docs/protocol.md` §4 and
 * `RunCompletionPayload` decoding per §5.6.
 *
 * This module is decode-only: encoding lives on the Rust side.
 * `decodeWireValue` is exported for testing; normal callers use
 * `decodeRunCompletionPayload` only.
 */

import { Buffer } from 'node:buffer'
import type { RunErrorCode, RunResult, SandboxExports } from './types'

// ── Value tags ─────────────────────────────────────────────────────────────

const TAG_UNDEFINED = 0x00
const TAG_NULL = 0x01
const TAG_FALSE = 0x02
const TAG_TRUE = 0x03
const TAG_NUMBER = 0x04
const TAG_STRING = 0x05
const TAG_BIGINT = 0x06
const TAG_BYTES = 0x07
const TAG_ARRAY = 0x08
const TAG_OBJECT = 0x09

// ── Error ──────────────────────────────────────────────────────────────────

export class WireDecodeError extends Error {
  override readonly name = 'WireDecodeError'
  constructor(message: string) {
    super(message)
  }
}

// ── Reader ─────────────────────────────────────────────────────────────────

/**
 * Stateful cursor over a byte buffer.
 * Every read advances the internal offset; out-of-bounds reads throw
 * `WireDecodeError` immediately so callers never see partial state.
 */
class WireReader {
  private readonly view: DataView
  private offset = 0

  constructor(buf: Uint8Array) {
    // Respect the byteOffset of sliced Uint8Arrays.
    this.view = new DataView(buf.buffer, buf.byteOffset, buf.byteLength)
  }

  get remaining(): number {
    return this.view.byteLength - this.offset
  }

  readU8(): number {
    if (this.remaining < 1) {
      throw new WireDecodeError('unexpected end of data reading u8')
    }
    return this.view.getUint8(this.offset++)
  }

  readU32(): number {
    if (this.remaining < 4) {
      throw new WireDecodeError('unexpected end of data reading u32')
    }
    const n = this.view.getUint32(this.offset, false) // big-endian
    this.offset += 4
    return n
  }

  readF64(): number {
    if (this.remaining < 8) {
      throw new WireDecodeError('unexpected end of data reading f64')
    }
    const n = this.view.getFloat64(this.offset, false) // big-endian
    this.offset += 8
    return n
  }

  /**
   * Read a big-endian u64 as a JS `bigint`. Uses two u32 reads to avoid
   *  floating-point precision loss from DataView.getBigUint64 on older runtimes
   *  (though Node 18+ supports it; this is belt-and-suspenders).
   */
  readU64BE(): bigint {
    if (this.remaining < 8) {
      throw new WireDecodeError('unexpected end of data reading u64')
    }
    const hi = BigInt(this.view.getUint32(this.offset, false))
    const lo = BigInt(this.view.getUint32(this.offset + 4, false))
    this.offset += 8
    return (hi << 32n) | lo
  }

  readRawBytes(len: number): Uint8Array {
    if (this.remaining < len) {
      throw new WireDecodeError(
        `unexpected end of data: need ${len} bytes, have ${this.remaining}`,
      )
    }
    const slice = new Uint8Array(
      this.view.buffer,
      this.view.byteOffset + this.offset,
      len,
    )
    this.offset += len
    return slice
  }

  readString(): string {
    const len = this.readU32()
    const bytes = this.readRawBytes(len)
    return new TextDecoder().decode(bytes)
  }

  readBool(): boolean {
    const b = this.readU8()
    if (b !== 0 && b !== 1) {
      throw new WireDecodeError(
        `invalid bool byte: 0x${b.toString(16).padStart(2, '0')}`,
      )
    }
    return b === 1
  }

  readStringList(): string[] {
    const count = this.readU32()
    const items: string[] = []
    for (let i = 0; i < count; i++) {
      items.push(this.readString())
    }
    return items
  }

  assertDone(): void {
    if (this.remaining !== 0) {
      throw new WireDecodeError(
        `${this.remaining} trailing bytes after expected end of payload`,
      )
    }
  }
}

// ── WireValue decoder ──────────────────────────────────────────────────────

function decodeWireValueFromReader(reader: WireReader): unknown {
  const tag = reader.readU8()
  switch (tag) {
    case TAG_UNDEFINED:
      return undefined
    case TAG_NULL:
      return null
    case TAG_FALSE:
      return false
    case TAG_TRUE:
      return true
    case TAG_NUMBER:
      return reader.readF64()
    case TAG_STRING:
      return reader.readString()
    case TAG_BIGINT: {
      const sign = reader.readU8() // 0 = non-negative, 1 = negative
      const count = reader.readU32() // number of u64 words (LSW first)
      let magnitude = 0n
      for (let i = 0; i < count; i++) {
        magnitude |= reader.readU64BE() << BigInt(i * 64)
      }
      return sign ? -magnitude : magnitude
    }
    case TAG_BYTES: {
      const len = reader.readU32()
      // .slice() copies the bytes so the caller owns independent memory.
      return reader.readRawBytes(len).slice()
    }
    case TAG_ARRAY: {
      const count = reader.readU32()
      const items: unknown[] = []
      for (let i = 0; i < count; i++) {
        items.push(decodeWireValueFromReader(reader))
      }
      return items
    }
    case TAG_OBJECT: {
      const count = reader.readU32()
      // Object.create(null) — no prototype chain, so any '__proto__' key
      // that survives is a plain data property rather than a [[Set]] that
      // triggers the __proto__ accessor on Object.prototype.
      // TODO: Object.create(null) has a known V8 hidden-class penalty vs {}.
      // Profile before optimising — security comes first; a null-prototype
      // map with string keys may eventually be replaced by a Map<string,unknown>
      // or a dedicated typed decoder once the hot path is identified.
      const obj = Object.create(null) as Record<string, unknown>
      for (let i = 0; i < count; i++) {
        const key = reader.readString()
        // Always decode the value to advance the reader, even for dropped keys.
        const val = decodeWireValueFromReader(reader)
        // Drop "__proto__" — defence-in-depth: Rust already elides it at
        // the encoding boundary (serialize_object_fields), but guard here
        // too so this decoder is self-contained and safe regardless.
        if (key === '__proto__')
          continue
        obj[key] = val
      }
      return obj
    }
    default: {
      const hex = tag.toString(16).padStart(2, '0')
      throw new WireDecodeError(`unknown WireValue tag: 0x${hex}`)
    }
  }
}

/**
 * Decode a single `WireValue` from the entire buffer.
 *
 * Throws `WireDecodeError` on unknown tags, truncated data, or trailing bytes.
 * Exported for testing - normal callers use `decodeRunCompletionPayload`.
 * @param buf
 */
export function decodeWireValue(buf: Uint8Array): unknown {
  const reader = new WireReader(buf)
  const value = decodeWireValueFromReader(reader)
  reader.assertDone()
  return value
}

/**
 * Decode a single `WireValue` from the start of `buf`, returning both the
 * decoded value and the number of bytes consumed.
 *
 * Used by `decodeBridgeCallPayload` in `ipc.ts` where multiple values are
 * packed sequentially in a larger payload buffer.
 * @param buf
 */
export function decodeWireValueFromSlice(buf: Uint8Array): [unknown, number] {
  const reader = new WireReader(buf)
  const value = decodeWireValueFromReader(reader)
  const consumed = buf.byteLength - reader.remaining
  return [value, consumed]
}

// ── RunCompletionPayload decoder ───────────────────────────────────────────

export interface DecodedRunCompletion {
  /**
   * Run identifier echoed from the `Run` request.
   * Phase 1: always `0`. Exposed for future request-response matching.
   */
  runId: number
  result: RunResult
}

/**
 * Decode a `RunCompletionPayload` from a `Result` frame payload.
 *
 * Wire layout per `docs/protocol.md` §5.6:
 * ```
 * u32   runId
 * u8    ok
 * u8    successPresent   (1 when ok = 1)
 *   WireValue  exports
 *   List<String>  stdout
 *   List<String>  stderr
 *   f64  durationMs
 * u8    failurePresent   (1 when ok = 0)
 *   String  code
 *   String  name
 *   String  message
 *   Optional<String>  stack
 *   List<String>  stdout
 *   List<String>  stderr
 *   f64  durationMs
 * ```
 * @param buf
 */
export function decodeRunCompletionPayload(buf: Uint8Array): DecodedRunCompletion {
  const reader = new WireReader(buf)
  const runId = reader.readU32()
  const ok = reader.readBool()

  if (ok) {
    const successPresent = reader.readU8()
    if (successPresent !== 1) {
      throw new WireDecodeError(
        'expected success present byte = 1 when ok = true',
      )
    }
    const exportsRaw = decodeWireValueFromReader(reader)
    const stdout = reader.readStringList()
    const stderr = reader.readStringList()
    const durationMs = reader.readF64()
    reader.readU8() // failurePresent = 0; consumed for forward-compat

    reader.assertDone()
    return {
      runId,
      result: {
        status: 'completed',
        ok: true,
        exports: wireObjectToExports(exportsRaw),
        stdout,
        stderr,
        durationMs,
      },
    }
  }

  reader.readU8() // successPresent = 0
  const failurePresent = reader.readU8()
  if (failurePresent !== 1) {
    throw new WireDecodeError(
      'expected failure present byte = 1 when ok = false',
    )
  }
  const code = reader.readString() as RunErrorCode
  const name = reader.readString()
  const message = reader.readString()
  const stackPresent = reader.readU8()
  const stack = stackPresent === 1 ? reader.readString() : undefined
  const dataPresent = reader.readU8()
  const data = dataPresent === 1 ? decodeWireValueFromReader(reader) : undefined
  const stdout = reader.readStringList()
  const stderr = reader.readStringList()
  const durationMs = reader.readF64()

  reader.assertDone()
  return {
    runId,
    result: { status: 'failed', ok: false, error: { code, name, message, stack, data }, stdout, stderr, durationMs },
  }
}

// ── PrecompileResultPayload decoder ───────────────────────────────────────────

export type PrecompileResult
  = | { ok: true, prefixId: string }
    | { ok: false, error: { code: string, name: string, message: string, stack?: string } }

/**
 * Decode a `PrecompileResultPayload` from a `PrecompileResult` frame.
 *
 * Wire layout per `docs/protocol.md` §5.6:
 * ```
 * u8    ok
 * u8    prefixIdPresent   (1 when ok = true)
 *   String  prefixId
 * u8    errorPresent      (1 when ok = false)
 *   RunErrorPayload  error
 * ```
 * @param buf
 */
export function decodePrecompileResultPayload(buf: Uint8Array): PrecompileResult {
  const reader = new WireReader(buf)
  const ok = reader.readBool()

  if (ok) {
    const prefixIdPresent = reader.readU8()
    if (prefixIdPresent !== 1) {
      throw new WireDecodeError('expected prefixId present byte = 1 when ok = true')
    }
    const prefixId = reader.readString()
    reader.readU8() // errorPresent = 0
    reader.assertDone()
    return { ok: true, prefixId }
  }

  reader.readU8() // prefixIdPresent = 0
  const errorPresent = reader.readU8()
  if (errorPresent !== 1) {
    throw new WireDecodeError('expected error present byte = 1 when ok = false')
  }
  const code = reader.readString()
  const name = reader.readString()
  const message = reader.readString()
  const stackPresent = reader.readU8()
  const stack = stackPresent === 1 ? reader.readString() : undefined
  const dataPresent = reader.readU8()
  if (dataPresent === 1)
    decodeWireValueFromReader(reader) // consume; precompile errors never carry data
  reader.assertDone()
  return { ok: false, error: { code, name, message, stack } }
}

// ── Helpers ────────────────────────────────────────────────────────────────

// ── WireValue encoder (host → Rust direction) ──────────────────────────────────
//
// Used to encode the host handler's return value in a BridgeResponse.

const ENC_TAG_UNDEFINED = 0x00
const ENC_TAG_NULL = 0x01
const ENC_TAG_FALSE = 0x02
const ENC_TAG_TRUE = 0x03
const ENC_TAG_NUMBER = 0x04
const ENC_TAG_STRING = 0x05
const ENC_TAG_BIGINT = 0x06
const ENC_TAG_BYTES = 0x07
const ENC_TAG_ARRAY = 0x08
const ENC_TAG_OBJECT = 0x09

class WireWriter {
  private readonly parts: Buffer[] = []
  /**
   * Objects currently on the recursion stack. Used for cycle detection.
   * Mirrors the `visiting` Vec in Rust's `value_to_wire`.
   *
   * Path-based (add before recursing, delete after): allows diamond shapes
   * (`{ a: x, b: x }` where x is the same object) but rejects true cycles.
   */
  private readonly visiting = new WeakSet<object>()

  writeU8(n: number): this {
    const b = Buffer.allocUnsafe(1)
    b.writeUInt8(n, 0)
    this.parts.push(b)
    return this
  }

  writeU32(n: number): this {
    const b = Buffer.allocUnsafe(4)
    b.writeUInt32BE(n, 0)
    this.parts.push(b)
    return this
  }

  writeF64(n: number): this {
    const b = Buffer.allocUnsafe(8)
    b.writeDoubleBE(n, 0)
    this.parts.push(b)
    return this
  }

  /**
   * Write a JS `bigint` as a big-endian u64 (must fit in 64 bits).
   * @param n
   */
  writeU64BE(n: bigint): this {
    const b = Buffer.allocUnsafe(8)
    b.writeUInt32BE(Number((n >> 32n) & 0xffffffffn), 0)
    b.writeUInt32BE(Number(n & 0xffffffffn), 4)
    this.parts.push(b)
    return this
  }

  writeString(s: string): this {
    const encoded = Buffer.from(s, 'utf8')
    this.writeU32(encoded.byteLength)
    this.parts.push(encoded)
    return this
  }

  writeValue(value: unknown): this {
    if (value === undefined) {
      return this.writeU8(ENC_TAG_UNDEFINED)
    }
    if (value === null) {
      return this.writeU8(ENC_TAG_NULL)
    }
    if (typeof value === 'boolean') {
      return this.writeU8(value ? ENC_TAG_TRUE : ENC_TAG_FALSE)
    }
    if (typeof value === 'number') {
      return this.writeU8(ENC_TAG_NUMBER).writeF64(value)
    }
    if (typeof value === 'string') {
      return this.writeU8(ENC_TAG_STRING).writeString(value)
    }
    if (typeof value === 'bigint') {
      // Encode as: sign_bit (u8) + word_count (u32) + words (u64 BE, LSW first).
      // Matches V8's new_from_words / to_words_array representation exactly.
      const sign = value < 0n ? 1 : 0
      let magnitude = value < 0n ? -value : value
      const words: bigint[] = []
      while (magnitude > 0n) {
        words.push(magnitude & 0xffffffffffffffffn)
        magnitude >>= 64n
      }
      this.writeU8(ENC_TAG_BIGINT)
      this.writeU8(sign)
      this.writeU32(words.length)
      for (const word of words) this.writeU64BE(word)
      return this
    }
    if (value instanceof Uint8Array) {
      this.writeU8(ENC_TAG_BYTES)
      this.writeU32(value.byteLength)
      this.parts.push(Buffer.from(value.buffer, value.byteOffset, value.byteLength))
      return this
    }
    if (Array.isArray(value)) {
      if (this.visiting.has(value)) {
        throw new TypeError(
          '[iso4] encodeWireValue: cyclic or self-referential structure',
        )
      }
      this.visiting.add(value)
      this.writeU8(ENC_TAG_ARRAY)
      this.writeU32(value.length)
      for (const item of value) {
        this.writeValue(item)
      }
      this.visiting.delete(value)
      return this
    }
    if (typeof value === 'object') {
      if (this.visiting.has(value)) {
        throw new TypeError(
          '[iso4] encodeWireValue: cyclic or self-referential structure',
        )
      }
      this.visiting.add(value)
      // Drop "__proto__" before computing the field count.
      // Object.entries on null-proto objects (e.g. those returned from
      // decodeWireValue) includes any own "__proto__" data property, so an
      // explicit guard is required to prevent it from crossing the boundary
      // in the host→sandbox direction.  Filter first so the count is correct.
      const entries = Object.entries(value as Record<string, unknown>)
        .filter(([k]) => k !== '__proto__')
      this.writeU8(ENC_TAG_OBJECT)
      this.writeU32(entries.length)
      for (const [k, v] of entries) {
        this.writeString(k)
        this.writeValue(v)
      }
      this.visiting.delete(value)
      return this
    }
    // Unrepresentable: function, symbol, Date, Map, Set, RegExp, class instance, etc.
    // Throw so the caller can send an error BridgeResponse rather than
    // silently injecting undefined into the sandbox.
    throw new TypeError(
      `[iso4] encodeWireValue: cannot encode value of type "${typeof value}"`,
    )
  }

  toBuffer(): Buffer {
    return Buffer.concat(this.parts)
  }
}

/**
 * Encode a single JavaScript value as a `WireValue` byte sequence.
 * Used to encode host bridge response return values.
 * @param value
 */
export function encodeWireValue(value: unknown): Buffer {
  return new WireWriter().writeValue(value).toBuffer()
}

function wireObjectToExports(raw: unknown): SandboxExports {
  if (raw === null || typeof raw !== 'object' || Array.isArray(raw)) {
    throw new WireDecodeError(
      `exports must decode to a WireValue::Object, got: ${
        Array.isArray(raw) ? 'array' : typeof raw
      }`,
    )
  }
  return raw as SandboxExports
}
