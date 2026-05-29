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
      const s = reader.readString()
      return BigInt(s)
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
      // Object.create(null) — no prototype chain, so obj['__proto__'] = x
      // is a plain data assignment rather than a [[Set]] that triggers the
      // __proto__ setter on Object.prototype. Prevents sandbox-controlled
      // keys from manipulating the decoded object's prototype chain.
      // TODO: Object.create(null) has a known V8 hidden-class penalty vs {}.
      // Profile before optimising — security comes first; a null-prototype
      // map with string keys may eventually be replaced by a Map<string,unknown>
      // or a dedicated typed decoder once the hot path is identified.
      const obj = Object.create(null) as Record<string, unknown>
      for (let i = 0; i < count; i++) {
        const key = reader.readString()
        obj[key] = decodeWireValueFromReader(reader)
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
  const stdout = reader.readStringList()
  const stderr = reader.readStringList()
  const durationMs = reader.readF64()

  reader.assertDone()
  return {
    runId,
    result: { ok: false, error: { code, name, message, stack }, stdout, stderr, durationMs },
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
      return this.writeU8(ENC_TAG_BIGINT).writeString(value.toString())
    }
    if (value instanceof Uint8Array) {
      this.writeU8(ENC_TAG_BYTES)
      this.writeU32(value.byteLength)
      this.parts.push(Buffer.from(value.buffer, value.byteOffset, value.byteLength))
      return this
    }
    if (Array.isArray(value)) {
      this.writeU8(ENC_TAG_ARRAY)
      this.writeU32(value.length)
      for (const item of value) {
        this.writeValue(item)
      }
      return this
    }
    if (typeof value === 'object') {
      const entries = Object.entries(value as Record<string, unknown>)
      this.writeU8(ENC_TAG_OBJECT)
      this.writeU32(entries.length)
      for (const [k, v] of entries) {
        this.writeString(k)
        this.writeValue(v)
      }
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
