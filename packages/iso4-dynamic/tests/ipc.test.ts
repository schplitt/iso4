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
} from '../src/ipc'

import { Buffer } from 'node:buffer'

// ── Payload encoders ───────────────────────────────────────────────────────

import {
  encodeRunPayload,
  encodePrecompilePayload,
  encodePrefixRunPayload,
  encodeDisposePrefixPayload,
} from '../src/ipc.js'

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
  test('auth payload roundtrip preserves protocol version and token', () => {
    const payload = encodeAuthenticatePayload({
      protocolVersion: PROTOCOL_VERSION,
      token: 'secret-token',
    })
    const auth = decodeAuthenticatePayload(payload)

    expect(auth).toEqual({
      protocolVersion: PROTOCOL_VERSION,
      token: 'secret-token',
    })
  })

  test('auth payload rejects too-short payload', () => {
    expect(() => decodeAuthenticatePayload(Buffer.from([0x00]))).toThrow(
      /payload too short for Authenticate/,
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
    const buf = encodeRunPayload({ runId: 1, code: 'x', filename: 'agent.js' })
    const codeEnd = 4 + 4 + 1
    expect(buf[codeEnd]).toBe(1) // filename present
    const { value: fn } = readString(buf, codeEnd + 1)
    expect(fn).toBe('agent.js')
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
})
