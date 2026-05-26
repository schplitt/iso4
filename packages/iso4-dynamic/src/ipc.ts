import { Buffer } from 'node:buffer'
import type { Socket } from 'node:net'

export const PROTOCOL_VERSION: 1 = 1

export const DEFAULT_MAX_FRAME_LENGTH: number = 64 * 1024 * 1024

export const TsToRustMessageTypes = {
  Authenticate: 0x01,
  Run: 0x02,
  BridgeResponse: 0x03,
  Terminate: 0x04,
} as const

export type TsToRustMessageType
  = (typeof TsToRustMessageTypes)[keyof typeof TsToRustMessageTypes]

export const RustToTsMessageTypes = {
  BridgeCall: 0x01,
  StdioChunk: 0x02,
  Result: 0x03,
  Log: 0x04,
} as const

export type RustToTsMessageType
  = (typeof RustToTsMessageTypes)[keyof typeof RustToTsMessageTypes]

export interface Frame {
  messageType: number
  payload: Uint8Array
}

export interface TypedFrame<MessageType extends number> {
  messageType: MessageType
  payload: Uint8Array
}

export type TsToRustFrame = TypedFrame<TsToRustMessageType>
export type RustToTsFrame = TypedFrame<RustToTsMessageType>

export interface AuthenticatePayload {
  protocolVersion: number
  token: string
}

interface PendingRead {
  resolve: (frame: Frame) => void
  reject: (error: Error) => void
}

export class FrameReader {
  private buffer: Buffer = Buffer.alloc(0)
  private readonly pendingReads: PendingRead[] = []
  private closedError: Error | null = null

  push(chunk: Uint8Array): void {
    if (this.closedError !== null) {
      return
    }

    this.buffer = Buffer.concat([
      this.buffer,
      Buffer.from(chunk.buffer, chunk.byteOffset, chunk.byteLength),
    ])
    this.flushPendingReads()
  }

  readFrame(): Promise<Frame> {
    const frame = this.tryReadFrame()
    if (frame !== null) {
      return Promise.resolve(frame)
    }

    if (this.closedError !== null) {
      return Promise.reject(this.closedError)
    }

    return new Promise((resolve, reject) => {
      this.pendingReads.push({ resolve, reject })
    })
  }

  async readRustToTsFrame(): Promise<RustToTsFrame> {
    const frame = await this.readFrame()
    return {
      messageType: parseRustToTsMessageType(frame.messageType),
      payload: frame.payload,
    }
  }

  close(error: Error = new Error('frame reader closed')): void {
    if (this.closedError !== null) {
      return
    }

    this.closedError = error
    this.flushPendingReads()

    while (this.pendingReads.length > 0) {
      this.pendingReads.shift()?.reject(error)
    }
  }

  private flushPendingReads(): void {
    while (this.pendingReads.length > 0) {
      const frame = this.tryReadFrame()
      if (frame === null) {
        break
      }

      this.pendingReads.shift()?.resolve(frame)
    }
  }

  private tryReadFrame(): Frame | null {
    if (this.buffer.byteLength < 4) {
      return null
    }

    const length = this.buffer.readUInt32BE(0)
    if (length === 0) {
      const error = new Error('frame length cannot be zero')
      this.close(error)
      throw error
    }
    if (length > DEFAULT_MAX_FRAME_LENGTH) {
      const error = new Error(
        `frame length ${length} exceeds max frame length ${DEFAULT_MAX_FRAME_LENGTH}`,
      )
      this.close(error)
      throw error
    }
    if (this.buffer.byteLength < 4 + length) {
      return null
    }

    const frameBytes = this.buffer.subarray(0, 4 + length)
    this.buffer = this.buffer.subarray(4 + length)
    return decodeFrame(frameBytes)
  }
}

export class SocketFrameReader {
  private readonly frames: FrameReader = new FrameReader()

  constructor(socket: Socket) {
    socket.on('data', (chunk: Buffer) => {
      this.frames.push(chunk)
    })
    socket.once('error', (error: Error) => {
      this.frames.close(error)
    })
    socket.once('end', () => {
      this.frames.close(new Error('socket ended'))
    })
    socket.once('close', () => {
      this.frames.close(new Error('socket closed'))
    })
  }

  readFrame(): Promise<Frame> {
    return this.frames.readFrame()
  }

  readRustToTsFrame(): Promise<RustToTsFrame> {
    return this.frames.readRustToTsFrame()
  }
}

export function encodeFrame(
  messageType: number,
  payload: Uint8Array,
  maxFrameLength: number = DEFAULT_MAX_FRAME_LENGTH,
): Buffer {
  if (!Number.isInteger(messageType) || messageType < 0 || messageType > 255) {
    throw new Error(`message type must fit in one byte: ${messageType}`)
  }

  const length = payload.byteLength + 1
  if (length > maxFrameLength) {
    throw new Error(
      `frame length ${length} exceeds max frame length ${maxFrameLength}`,
    )
  }

  const frame = Buffer.allocUnsafe(4 + length)
  frame.writeUInt32BE(length, 0)
  frame[4] = messageType
  Buffer.from(payload.buffer, payload.byteOffset, payload.byteLength).copy(
    frame,
    5,
  )
  return frame
}

export function decodeFrame(
  bytes: Uint8Array,
  maxFrameLength: number = DEFAULT_MAX_FRAME_LENGTH,
): Frame {
  if (bytes.byteLength < 4) {
    throw new Error('frame is missing 4-byte length prefix')
  }

  const view = Buffer.from(bytes.buffer, bytes.byteOffset, bytes.byteLength)
  const length = view.readUInt32BE(0)
  if (length === 0) {
    throw new Error('frame length cannot be zero')
  }
  if (length > maxFrameLength) {
    throw new Error(
      `frame length ${length} exceeds max frame length ${maxFrameLength}`,
    )
  }
  if (view.byteLength < 4 + length) {
    throw new Error('frame body is truncated')
  }
  if (view.byteLength > 4 + length) {
    throw new Error('buffer contains trailing bytes after frame')
  }

  const messageType = view[4]
  if (messageType === undefined) {
    throw new Error('frame is missing message type')
  }

  return {
    messageType,
    payload: view.subarray(5),
  }
}

export function encodeTsToRustFrame(
  messageType: TsToRustMessageType,
  payload: Uint8Array,
): Buffer {
  return encodeFrame(messageType, payload)
}

export function encodeRustToTsFrame(
  messageType: RustToTsMessageType,
  payload: Uint8Array,
): Buffer {
  return encodeFrame(messageType, payload)
}

export function decodeTsToRustFrame(bytes: Uint8Array): TsToRustFrame {
  const frame = decodeFrame(bytes)
  return {
    messageType: parseTsToRustMessageType(frame.messageType),
    payload: frame.payload,
  }
}

export function decodeRustToTsFrame(bytes: Uint8Array): RustToTsFrame {
  const frame = decodeFrame(bytes)
  return {
    messageType: parseRustToTsMessageType(frame.messageType),
    payload: frame.payload,
  }
}

export function encodeAuthenticatePayload(
  auth: AuthenticatePayload,
): Buffer {
  if (!Number.isInteger(auth.protocolVersion)) {
    throw new Error('protocolVersion must be an integer')
  }
  if (auth.protocolVersion < 0 || auth.protocolVersion > 0xffff) {
    throw new Error(`protocolVersion out of u16 range: ${auth.protocolVersion}`)
  }

  const token = Buffer.from(auth.token, 'utf8')
  const payload = Buffer.allocUnsafe(2 + token.byteLength)
  payload.writeUInt16BE(auth.protocolVersion, 0)
  token.copy(payload, 2)
  return payload
}

export function decodeAuthenticatePayload(
  payload: Uint8Array,
): AuthenticatePayload {
  if (payload.byteLength < 2) {
    throw new Error('payload too short for Authenticate')
  }

  const view = Buffer.from(payload.buffer, payload.byteOffset, payload.byteLength)
  return {
    protocolVersion: view.readUInt16BE(0),
    token: view.subarray(2).toString('utf8'),
  }
}

export function parseTsToRustMessageType(
  byte: number,
): TsToRustMessageType {
  switch (byte) {
    case TsToRustMessageTypes.Authenticate:
    case TsToRustMessageTypes.Run:
    case TsToRustMessageTypes.BridgeResponse:
    case TsToRustMessageTypes.Terminate:
      return byte
    default:
      throw new Error(`unknown TS->Rust message type: ${formatByte(byte)}`)
  }
}

export function parseRustToTsMessageType(
  byte: number,
): RustToTsMessageType {
  switch (byte) {
    case RustToTsMessageTypes.BridgeCall:
    case RustToTsMessageTypes.StdioChunk:
    case RustToTsMessageTypes.Result:
    case RustToTsMessageTypes.Log:
      return byte
    default:
      throw new Error(`unknown Rust->TS message type: ${formatByte(byte)}`)
  }
}

function formatByte(byte: number): string {
  return `0x${byte.toString(16).padStart(2, '0')}`
}
