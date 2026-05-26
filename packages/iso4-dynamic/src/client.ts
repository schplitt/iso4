import type { Buffer } from 'node:buffer'
import { createConnection } from 'node:net'
import type { Socket } from 'node:net'
import {
  FrameReader,
  PROTOCOL_VERSION,

  RustToTsMessageTypes,
  TsToRustMessageTypes,
  encodeAuthenticatePayload,
  encodeDisposePrefixPayload,
  encodePrecompilePayload,
  encodePrefixRunPayload,
  encodeRunPayload,
  encodeTsToRustFrame,
} from './ipc'
import type { PrecompilePayloadOptions, PrefixRunPayloadOptions, ResourceLimits, RunPayloadOptions } from './ipc'

export interface RuntimeIpcClientOptions {
  socketPath: string
  token: string
}

export interface RawRunResult {
  result: Uint8Array
}

export type { ResourceLimits, RunPayloadOptions, PrecompilePayloadOptions, PrefixRunPayloadOptions }

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
    const socket = await connectSocket(options.socketPath)
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

  private nextRunId = 0
  private nextRunIdValue(): number {
    this.nextRunId = (this.nextRunId + 1) & 0xffffffff
    return this.nextRunId
  }

  async runRawCode(
    code: string,
    options?: { filename?: string, limits?: ResourceLimits },
  ): Promise<RawRunResult> {
    if (this.disposed) {
      throw new Error('runtime IPC client is disposed')
    }

    await this.write(
      encodeTsToRustFrame(
        TsToRustMessageTypes.Run,
        encodeRunPayload({
          runId: this.nextRunIdValue(),
          code,
          filename: options?.filename,
          limits: options?.limits,
        }),
      ),
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

  async precompile(
    options: PrecompilePayloadOptions,
  ): Promise<Uint8Array> {
    if (this.disposed)
      throw new Error('runtime IPC client is disposed')

    await this.write(
      encodeTsToRustFrame(
        TsToRustMessageTypes.Precompile,
        encodePrecompilePayload(options),
      ),
    )

    for await (const frame of this.reader) {
      if (frame.messageType === RustToTsMessageTypes.PrecompileResult) {
        return frame.payload
      }
      if (frame.messageType === RustToTsMessageTypes.Log)
        continue
    }

    throw new Error('connection closed before receiving a PrecompileResult frame')
  }

  async prefixRun(
    options: PrefixRunPayloadOptions,
  ): Promise<RawRunResult> {
    if (this.disposed)
      throw new Error('runtime IPC client is disposed')

    await this.write(
      encodeTsToRustFrame(
        TsToRustMessageTypes.PrefixRun,
        encodePrefixRunPayload({
          ...options,
          runId: this.nextRunIdValue(),
        }),
      ),
    )

    for await (const frame of this.reader) {
      switch (frame.messageType) {
        case RustToTsMessageTypes.Result:
          return { result: frame.payload }
        case RustToTsMessageTypes.Log:
          break
        case RustToTsMessageTypes.BridgeCall:
          throw new Error('BridgeCall is not implemented in raw IPC client')
      }
    }

    throw new Error('connection closed before receiving a Result frame')
  }

  async disposePrefix(prefixId: string): Promise<void> {
    if (this.disposed)
      return
    await this.write(
      encodeTsToRustFrame(
        TsToRustMessageTypes.DisposePrefix,
        encodeDisposePrefixPayload(prefixId),
      ),
    )
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
