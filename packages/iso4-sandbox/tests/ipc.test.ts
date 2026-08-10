import { describe, expect, test } from 'vitest'
import {
  FrameReader,
  PROTOCOL_VERSION,
  RustToTsMessageTypes,
  TsToRustMessageTypes,
  decodeAuthenticatePayload,
  decodeFrame,
  decodeRustToTsFrame,
  decodeTsToRustFrame,
  encodeAuthenticatePayload,
  encodeFrame,
  encodeRustToTsFrame,
  encodeTsToRustFrame,
  parseRustToTsMessageType,
  parseTsToRustMessageType,
  encodeRunPayload,
  encodePrecompilePayload,
  encodePrefixRunPayload,
  encodeDisposePrefixPayload,
  bridgeErrorPayloadFromUnknown,
  decodeHelloPayload,
  encodeBridgeResponsePayload,
  encodeHelloPayload,
  HelloStatus,
} from '../src/ipc'
import { deserializeValue, serializationProbe, serializeValue } from '../src/v8-codec'

import { Buffer } from 'node:buffer'

// ── Payload encoders ───────────────────────────────────────────────────────

describe('ipc frame codec', () => {
  test('frame roundtrip preserves type and payload', () => {
    const bytes = encodeFrame(0x02, Buffer.from('hello'))
    const frame = decodeFrame(bytes)

    expect(frame.messageType).toBe(0x02)
    expect(Buffer.from(frame.payload).toString('utf8')).toBe('hello')
  })

  test('empty payload frame has length 1', () => {
    const bytes = encodeFrame(0x04, new Uint8Array())

    expect(bytes.readUInt32BE(0)).toBe(1)
    expect(bytes[4]).toBe(0x04)
    expect(decodeFrame(bytes).payload.byteLength).toBe(0)
  })

  test('decode rejects zero-length frame', () => {
    expect(() => decodeFrame(Buffer.from([0, 0, 0, 0]))).toThrow(
      /frame length cannot be zero/,
    )
  })

  test('decode rejects truncated body', () => {
    expect(() => decodeFrame(Buffer.from([0, 0, 0, 3, 0x01, 0x02]))).toThrow(
      /frame body is truncated/,
    )
  })

  test('decode rejects frame larger than limit', () => {
    expect(() => decodeFrame(Buffer.from([0, 0, 0, 2, 0x01, 0x02]), 1)).toThrow(
      /frame length 2 exceeds max frame length 1/,
    )
  })

  test('encode rejects frame larger than limit', () => {
    expect(() => encodeFrame(0x01, Buffer.from([0x02]), 1)).toThrow(
      /frame length 2 exceeds max frame length 1/,
    )
  })
})

describe('typed frame codec', () => {
  test('TS->Rust typed frame roundtrip', () => {
    const bytes = encodeTsToRustFrame(TsToRustMessageTypes.Run, Buffer.from('x'))
    const frame = decodeTsToRustFrame(bytes)

    expect(frame.messageType).toBe(TsToRustMessageTypes.Run)
    expect(Buffer.from(frame.payload).toString('utf8')).toBe('x')
  })

  test('Rust->TS typed frame roundtrip', () => {
    const bytes = encodeRustToTsFrame(RustToTsMessageTypes.Result, Buffer.from('x'))
    const frame = decodeRustToTsFrame(bytes)

    expect(frame.messageType).toBe(RustToTsMessageTypes.Result)
    expect(Buffer.from(frame.payload).toString('utf8')).toBe('x')
  })

  test('TS->Rust rejects unknown message type', () => {
    expect(() => decodeTsToRustFrame(encodeFrame(0xff, new Uint8Array()))).toThrow(
      /unknown TS->Rust message type: 0xff/,
    )
  })

  test('Rust->TS rejects unknown message type', () => {
    expect(() => decodeRustToTsFrame(encodeFrame(0xff, new Uint8Array()))).toThrow(
      /unknown Rust->TS message type: 0xff/,
    )
  })

  test('message type parsers accept known values', () => {
    expect(parseTsToRustMessageType(0x01)).toBe(TsToRustMessageTypes.Authenticate)
    expect(parseTsToRustMessageType(0x03)).toBe(TsToRustMessageTypes.Precompile)
    expect(parseTsToRustMessageType(0x06)).toBe(TsToRustMessageTypes.BridgeResponse)
    expect(parseTsToRustMessageType(0x07)).toBe(TsToRustMessageTypes.Terminate)
    expect(parseRustToTsMessageType(0x02)).toBe(RustToTsMessageTypes.Result)
    expect(parseRustToTsMessageType(0x03)).toBe(RustToTsMessageTypes.PrecompileResult)
  })
})

describe('buffered frame reader', () => {
  test('waits for a split frame before resolving', async () => {
    const reader = new FrameReader()
    const bytes = encodeRustToTsFrame(RustToTsMessageTypes.Result, Buffer.from('done'))

    let settled = false
    const pending = reader.readRustToTsFrame().then((frame) => {
      settled = true
      return frame
    })

    reader.push(bytes.subarray(0, 2))
    await Promise.resolve()
    expect(settled).toBe(false)

    reader.push(bytes.subarray(2))
    const frame = await pending
    expect(frame.messageType).toBe(RustToTsMessageTypes.Result)
    expect(Buffer.from(frame.payload).toString('utf8')).toBe('done')
  })

  test('reads merged frames one at a time', async () => {
    const reader = new FrameReader()
    const first = encodeRustToTsFrame(RustToTsMessageTypes.Log, Buffer.from([0x01]))
    const second = encodeRustToTsFrame(RustToTsMessageTypes.Result, Buffer.from('ok'))

    reader.push(Buffer.concat([first, second]))

    const a = await reader.readRustToTsFrame()
    const b = await reader.readRustToTsFrame()

    expect(a.messageType).toBe(RustToTsMessageTypes.Log)
    expect([...a.payload]).toEqual([0x01])
    expect(b.messageType).toBe(RustToTsMessageTypes.Result)
    expect(Buffer.from(b.payload).toString('utf8')).toBe('ok')
  })

  test('rejects pending reads when closed', async () => {
    const reader = new FrameReader()
    const pending = reader.readFrame()
    reader.close(new Error('boom'))

    await expect(pending).rejects.toThrow(/boom/)
  })
})

describe('Authenticate payload', () => {
  test('auth payload roundtrip preserves protocol version, probe and token', () => {
    const probe = serializationProbe()
    const payload = encodeAuthenticatePayload({
      protocolVersion: PROTOCOL_VERSION,
      probe,
      token: 'secret-token',
    })
    const auth = decodeAuthenticatePayload(payload)

    expect(auth.protocolVersion).toBe(PROTOCOL_VERSION)
    expect(auth.token).toBe('secret-token')
    expect(Buffer.from(auth.probe)).toEqual(probe)
  })

  test('the probe is a serialized null carrying the format version', () => {
    const probe = serializationProbe()
    expect(probe[0]).toBe(0xFF) // V8 serialization header tag
    expect(probe[1]).toBeGreaterThan(0) // format version
    expect(deserializeValue(probe)).toBeNull()
  })

  test('auth payload rejects too-short payload', () => {
    expect(() => decodeAuthenticatePayload(Buffer.from([0x00]))).toThrow(
      /payload too short for Authenticate/,
    )
  })

  test('auth payload rejects a probe length past the end', () => {
    const payload = Buffer.from([0x00, 0x02, 0x00, 0x00, 0x00, 0x63, 0xFF])
    expect(() => decodeAuthenticatePayload(payload)).toThrow(
      /Authenticate payload truncated/,
    )
  })
})

describe('Hello payload', () => {
  test('roundtrip preserves status, probe and message', () => {
    const probe = serializationProbe()
    const hello = decodeHelloPayload(
      encodeHelloPayload({
        status: HelloStatus.V8FormatMismatch,
        probe,
        message: 'format mismatch',
      }),
    )
    expect(hello.status).toBe(HelloStatus.V8FormatMismatch)
    expect(Buffer.from(hello.probe)).toEqual(probe)
    expect(hello.message).toBe('format mismatch')
  })

  test('an accepting Hello carries an empty message', () => {
    const hello = decodeHelloPayload(
      encodeHelloPayload({ status: HelloStatus.Ok, probe: serializationProbe(), message: '' }),
    )
    expect(hello.status).toBe(HelloStatus.Ok)
    expect(hello.message).toBe('')
  })

  test('rejects a truncated payload', () => {
    expect(() => decodeHelloPayload(Buffer.from([0x00, 0x00]))).toThrow(
      /payload too short for Hello/,
    )
  })
})

function readU32BE(buf: Buffer, offset: number): number {
  return buf.readUInt32BE(offset)
}
function readString(buf: Buffer, offset: number): { value: string, end: number } {
  const len = readU32BE(buf, offset)
  return { value: buf.subarray(offset + 4, offset + 4 + len).toString('utf8'), end: offset + 4 + len }
}

describe('payload encoders', () => {
  test('encodeRunPayload encodes runId and code correctly', () => {
    const buf = encodeRunPayload({ runId: 7, code: 'export default 1' })
    expect(readU32BE(buf, 0)).toBe(7) // runId
    const { value: code } = readString(buf, 4)
    expect(code).toBe('export default 1')
  })

  test('encodeRunPayload with no filename writes absent byte', () => {
    const buf = encodeRunPayload({ runId: 1, code: 'x' })
    const codeEnd = 4 + 4 + 1 // runId + codeLen + 1 char
    expect(buf[codeEnd]).toBe(0) // filename absent
  })

  test('encodeRunPayload with filename writes present byte and string', () => {
    const buf = encodeRunPayload({ runId: 1, code: 'x', filename: 'agent' })
    const codeEnd = 4 + 4 + 1
    expect(buf[codeEnd]).toBe(1) // filename present
    const { value: fn } = readString(buf, codeEnd + 1)
    expect(fn).toBe('agent')
  })

  test('encodePrecompilePayload has no runId prefix', () => {
    const runBuf = encodeRunPayload({ runId: 0, code: 'x' })
    const preBuf = encodePrecompilePayload({ code: 'x' })
    // PrecompilePayload is 4 bytes shorter (no runId)
    expect(preBuf.byteLength).toBe(runBuf.byteLength - 4)
    const { value: code } = readString(preBuf, 0)
    expect(code).toBe('x')
  })

  test('encodePrefixRunPayload encodes runId, prefixId, and code', () => {
    const buf = encodePrefixRunPayload({ runId: 3, prefixId: '42', code: 'y' })
    expect(readU32BE(buf, 0)).toBe(3) // runId
    const { value: prefixId, end } = readString(buf, 4)
    expect(prefixId).toBe('42')
    const { value: code } = readString(buf, end)
    expect(code).toBe('y')
  })

  test('encodeDisposePrefixPayload encodes just the prefixId string', () => {
    const buf = encodeDisposePrefixPayload('99')
    const { value } = readString(buf, 0)
    expect(value).toBe('99')
    expect(buf.byteLength).toBe(4 + 2) // u32 len + '99'
  })

  // ── imports field wire encoding ────────────────────────────────────────
  // Each binding is `String specifier` + `u8 kind`: source modules (kind 0)
  // carry ESM text; host modules (kind 1) carry their shape as a data tree.
  // The Rust parser is in `native/v8-runtime/src/ipc.rs`; the round-trip
  // tests there mirror the layout asserted below.

  test('encodeRunPayload with one source import lays out specifier, kind, source', () => {
    const buf = encodeRunPayload({
      runId: 1,
      code: 'x',
      imports: [
        { specifier: 'lib:math', source: 'export const add = (a, b) => a + b' },
      ],
    })
    // Skip runId(4) + code(4+1) + filename absent(1) + limits(8) + globals count(4)
    let off = 4 + 4 + 1 + 1 + 8 + 4
    expect(readU32BE(buf, off)).toBe(1) // imports count
    off += 4
    const { value: specifier, end: e1 } = readString(buf, off)
    expect(specifier).toBe('lib:math')
    expect(buf[e1]).toBe(0) // kind: source
    const { value: source, end: e2 } = readString(buf, e1 + 1)
    expect(source).toBe('export const add = (a, b) => a + b')
    expect(e2).toBe(buf.byteLength)
  })

  test('encodeRunPayload lays out a host-module tree as tagged nodes', () => {
    const buf = encodeRunPayload({
      runId: 1,
      code: 'x',
      imports: [
        {
          specifier: 'tools:search',
          module: [
            ['query', { kind: 'function' }],
            ['limit', { kind: 'data', value: true }],
            ['nested', { kind: 'object', entries: [['inner', { kind: 'function' }]] }],
          ],
        },
      ],
    })
    let off = 4 + 4 + 1 + 1 + 8 + 4
    expect(readU32BE(buf, off)).toBe(1) // imports count
    off += 4
    const { value: specifier, end: e1 } = readString(buf, off)
    expect(specifier).toBe('tools:search')
    off = e1
    expect(buf[off]).toBe(1) // kind: host
    off += 1
    expect(readU32BE(buf, off)).toBe(3) // export count
    off += 4
    const { value: n1, end: e2 } = readString(buf, off)
    expect(n1).toBe('query')
    expect(buf[e2]).toBe(0) // node tag: function
    off = e2 + 1
    const { value: n2, end: e3 } = readString(buf, off)
    expect(n2).toBe('limit')
    expect(buf[e3]).toBe(1) // node tag: data
    // Value slot: u32 byteLength + blob (no tag byte — one codec).
    const dataLength = readU32BE(buf, e3 + 1)
    expect(deserializeValue(buf.subarray(e3 + 5, e3 + 5 + dataLength))).toBe(true)
    off = e3 + 5 + dataLength
    const { value: n3, end: e4 } = readString(buf, off)
    expect(n3).toBe('nested')
    expect(buf[e4]).toBe(2) // node tag: object
    expect(readU32BE(buf, e4 + 1)).toBe(1) // 1 child
    const { value: c1, end: e5 } = readString(buf, e4 + 5)
    expect(c1).toBe('inner')
    expect(buf[e5]).toBe(0) // node tag: function
    expect(e5 + 1).toBe(buf.byteLength)
  })

  test('encodeRunPayload with multiple imports preserves order', () => {
    const buf = encodeRunPayload({
      runId: 1,
      code: 'x',
      imports: [
        { specifier: 'lib:a', source: 'export const a = 1' },
        { specifier: 'lib:b', source: 'export const b = 2' },
      ],
    })
    let off = 4 + 4 + 1 + 1 + 8 + 4
    expect(readU32BE(buf, off)).toBe(2) // imports count
    off += 4
    const { value: s1, end: e1 } = readString(buf, off)
    expect(s1).toBe('lib:a')
    expect(buf[e1]).toBe(0) // kind: source
    const { value: src1, end: e2 } = readString(buf, e1 + 1)
    expect(src1).toBe('export const a = 1')
    const { value: s2, end: e3 } = readString(buf, e2)
    expect(s2).toBe('lib:b')
    expect(buf[e3]).toBe(0) // kind: source
    const { value: src2, end: e4 } = readString(buf, e3 + 1)
    expect(src2).toBe('export const b = 2')
    expect(e4).toBe(buf.byteLength)
  })

  test('encodePrecompilePayload carries import declarations; encodePrefixRunPayload carries rebind locations', () => {
    const imports = [{ specifier: 'lib:zod', source: 'export const z = {}' }] as const
    const pre = encodePrecompilePayload({ code: 'x', imports })
    expect(pre.toString('utf8')).toContain('lib:zod')

    const pref = encodePrefixRunPayload({
      runId: 1,
      prefixId: 'p',
      code: 'x',
      importRebinds: [{ specifier: 'tools:search', path: 'nested.inner' }],
    })
    // Tail: u32 count, String specifier, String path.
    const s = pref.toString('utf8')
    expect(s).toContain('tools:search')
    expect(s).toContain('nested.inner')
  })

  test('imports field defaults to empty list when omitted', () => {
    const buf = encodeRunPayload({ runId: 1, code: 'x' })
    // Last 4 bytes should be u32(0) (imports count).
    expect(readU32BE(buf, buf.byteLength - 4)).toBe(0)
  })
})

// ── BridgeResponse error payloads ────────────────────────────────────────────

describe('bridgeErrorPayloadFromUnknown', () => {
  test('preserves the real error name and message', () => {
    const payload = bridgeErrorPayloadFromUnknown(new TypeError('bad input'))
    expect(payload.name).toBe('TypeError')
    expect(payload.message).toBe('bad input')
    expect(payload.encodedFields).toBeUndefined()
  })

  test('collects own-enumerable props as fields, dropping non-serializable ones', () => {
    const err = Object.assign(new Error('x'), {
      code: 'E_FOO',
      attempt: 2,
      onRetry: () => {}, // function — dropped
    })
    const payload = bridgeErrorPayloadFromUnknown(err)
    expect(payload.name).toBe('Error')
    expect(payload.encodedFields).toBeDefined()
    expect(deserializeValue(payload.encodedFields!)).toEqual({ code: 'E_FOO', attempt: 2 })
  })

  test('never carries name/message/stack inside fields', () => {
    const err = new Error('x')
    Object.defineProperty(err, 'stack', { value: 'secret host stack', enumerable: true })
    const payload = bridgeErrorPayloadFromUnknown(err)
    expect(payload.encodedFields).toBeUndefined()
  })

  test('thrown primitives become a generic Error payload', () => {
    expect(bridgeErrorPayloadFromUnknown('boom')).toEqual({ name: 'Error', message: 'boom' })
    expect(bridgeErrorPayloadFromUnknown(42)).toEqual({ name: 'Error', message: '42' })
  })

  test('throwing getters are dropped without failing the payload', () => {
    const err = new Error('x')
    Object.defineProperty(err, 'evil', {
      enumerable: true,
      get() {
        throw new Error('gotcha')
      },
    })
    const payload = bridgeErrorPayloadFromUnknown(err)
    expect(payload.message).toBe('x')
    expect(payload.encodedFields).toBeUndefined()
  })
})

describe('encodeBridgeResponsePayload error layout', () => {
  test('writes code, name, message, absent stack, and fields per §5.4', () => {
    const encodedFields = serializeValue({ code: 'E_FOO' })
    const buf = encodeBridgeResponsePayload(7, false, undefined, {
      name: 'WorkflowTimeout',
      message: 'took too long',
      encodedFields,
    })
    let off = 0
    expect(readU32BE(buf, off)).toBe(7) // callId
    off += 4
    expect(buf[off]).toBe(0) // ok = false
    off += 1
    const { value: code, end: e1 } = readString(buf, off)
    expect(code).toBe('ERR_HOST_BRIDGE')
    const { value: name, end: e2 } = readString(buf, e1)
    expect(name).toBe('WorkflowTimeout')
    const { value: message, end: e3 } = readString(buf, e2)
    expect(message).toBe('took too long')
    expect(buf[e3]).toBe(0) // stack: always absent host → sandbox
    expect(buf[e3 + 1]).toBe(1) // fields present
    // Value slot: u32 byteLength + blob.
    const fieldsLength = readU32BE(buf, e3 + 2)
    const fieldsBlob = buf.subarray(e3 + 6, e3 + 6 + fieldsLength)
    expect(deserializeValue(fieldsBlob)).toEqual({ code: 'E_FOO' })
  })

  test('omits fields when the error has none', () => {
    const buf = encodeBridgeResponsePayload(1, false, undefined, {
      name: 'Error',
      message: 'plain failure',
    })
    expect(buf[buf.byteLength - 1]).toBe(0) // fields absent is the last byte
  })
})
