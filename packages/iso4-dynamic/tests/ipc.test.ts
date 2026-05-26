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
    expect(parseRustToTsMessageType(0x03)).toBe(RustToTsMessageTypes.Result)
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
    const first = encodeRustToTsFrame(RustToTsMessageTypes.StdioChunk, Buffer.from([0, 97]))
    const second = encodeRustToTsFrame(RustToTsMessageTypes.Result, Buffer.from('ok'))

    reader.push(Buffer.concat([first, second]))

    const a = await reader.readRustToTsFrame()
    const b = await reader.readRustToTsFrame()

    expect(a.messageType).toBe(RustToTsMessageTypes.StdioChunk)
    expect([...a.payload]).toEqual([0, 97])
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
