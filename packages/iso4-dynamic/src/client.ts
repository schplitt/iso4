import { Buffer } from 'node:buffer'
import { createConnection } from 'node:net'
import type { Socket } from 'node:net'
import {
  FrameReader,
  PROTOCOL_VERSION,
  RustToTsMessageTypes,
  TsToRustMessageTypes,
  encodeAuthenticatePayload,
  encodeTsToRustFrame,
} from './ipc'

export const DEFAULT_SOCKET_PATH: string = '/tmp/iso4-dynamic-v8.sock'

export interface RuntimeIpcClientOptions {
  socketPath?: string
  token: string
}

export interface RawRunResult {
  result: Uint8Array
}

export class RuntimeIpcClient {
  private readonly socket: Socket
  private readonly reader: FrameReader
  private disposed = false

  private constructor(socket: Socket) {
    this.socket = socket
    this.reader = new FrameReader()
    socket.on('data', (chunk: Buffer) => {
      this.reader.push(chunk)
    })
    socket.once('error', (error: Error) => {
      this.reader.close(error)
    })
    socket.once('end', () => {
      this.reader.close(new Error('socket ended'))
    })
    socket.once('close', () => {
      this.reader.close(new Error('socket closed'))
    })
  }

  static async connect(options: RuntimeIpcClientOptions): Promise<RuntimeIpcClient> {
    const socket = await connectSocket(options.socketPath ?? DEFAULT_SOCKET_PATH)
    const client = new RuntimeIpcClient(socket)

    await client.write(
      encodeTsToRustFrame(
        TsToRustMessageTypes.Authenticate,
        encodeAuthenticatePayload({
          protocolVersion: PROTOCOL_VERSION,
          token: options.token,
        }),
      ),
    )

    return client
  }

  async runRawCode(code: string): Promise<RawRunResult> {
    if (this.disposed) {
      throw new Error('runtime IPC client is disposed')
    }

    await this.write(
      encodeTsToRustFrame(TsToRustMessageTypes.Run, Buffer.from(code, 'utf8')),
    )

    for await (const frame of this.reader) {
      switch (frame.messageType) {
        case RustToTsMessageTypes.Result:
          return { result: frame.payload }

        case RustToTsMessageTypes.Log:
          // Runtime diagnostics are intentionally ignored for the raw helper.
          // The real Runtime can forward these to a configured logger later.
          break

        case RustToTsMessageTypes.BridgeCall:
          throw new Error('BridgeCall is not implemented in raw IPC client')
      }
    }

    throw new Error('connection closed before receiving a Result frame')
  }

  async dispose(): Promise<void> {
    if (this.disposed) {
      return
    }

    this.disposed = true
    this.socket.end()
    await new Promise<void>((resolve) => {
      if (this.socket.destroyed) {
        resolve()
        return
      }
      this.socket.once('close', () => resolve())
      this.socket.destroy()
    })
  }

  private async write(frame: Buffer): Promise<void> {
    await new Promise<void>((resolve, reject) => {
      this.socket.write(frame, (error?: Error | null) => {
        if (error) {
          reject(error)
          return
        }
        resolve()
      })
    })
  }
}

function connectSocket(socketPath: string): Promise<Socket> {
  return new Promise((resolve, reject) => {
    const socket = createConnection(socketPath)

    socket.once('connect', () => {
      socket.off('error', reject)
      resolve(socket)
    })
    socket.once('error', reject)
  })
}
