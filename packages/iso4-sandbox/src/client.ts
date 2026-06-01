import type { Buffer } from 'node:buffer'
import { createConnection } from 'node:net'
import type { Socket } from 'node:net'
import {
  FrameReader,
  PROTOCOL_VERSION,

  RustToTsMessageTypes,
  TsToRustMessageTypes,
  decodeBridgeCallPayload,
  encodeBridgeResponsePayload,
  encodeAuthenticatePayload,
  encodeDisposePrefixPayload,
  encodePrecompilePayload,
  encodePrefixRunPayload,
  encodeRunPayload,
  encodeTsToRustFrame,
} from './ipc'
import type { HostExportFunction } from './types.js'
import type { ResourceLimits } from './ipc'
import { encodeWireValue } from './wire'

export interface RuntimeIpcClientOptions {
  socketPath: string
  token: string
}

export interface RawRunResult {
  result: Uint8Array
}

/**
 * Dispatches a bridge call to the host-configured handler.
 * Arguments are raw deserialized values; return value is re-serialized as WireValue.
 */
export type BridgeCallDispatcher = (
  exportName: string,
  args: unknown[],
) => Promise<unknown>

export type { ResourceLimits }

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
    // Wraps at 2³²-1 back to 0. Safe in v1 because runs are sequential per
    // connection — no two runs share a connection simultaneously, so a
    // rolled-over ID can never collide with a live one. When D11 (concurrent
    // async bridge) lands, add a `do { ... } while (inFlightRuns.has(id))`
    // guard here.
    this.nextRunId = (this.nextRunId + 1) & 0xffffffff
    return this.nextRunId
  }

  // globals here is always `Record<string, HostExportFunction>` — the
  // processed bridge map produced by `processGlobals()` in index.ts.
  // String globals become preamble code prepended to `code`; BridgeWithShim
  // handlers are unwrapped to private `__iso4_<name>_h` keys. The client
  // layer only ever sees plain bridge functions.
  async runRawCode(
    code: string,
    options?: { filename?: string, limits?: ResourceLimits, globals?: Record<string, HostExportFunction> },
  ): Promise<RawRunResult> {
    if (this.disposed)
      throw new Error('runtime IPC client is disposed')

    const globals = options?.globals ?? {}
    const globalNames = Object.keys(globals)
    await this.write(
      encodeTsToRustFrame(
        TsToRustMessageTypes.Run,
        encodeRunPayload({
          runId: this.nextRunIdValue(),
          code,
          filename: options?.filename,
          limits: options?.limits,
          globals: globalNames,
        }),
      ),
    )

    return this.drainUntilResult(makeDispatcher(globals))
  }

  async precompile(
    options: { code: string, filename?: string, limits?: ResourceLimits, globals?: Record<string, HostExportFunction> },
  ): Promise<Uint8Array> {
    if (this.disposed)
      throw new Error('runtime IPC client is disposed')

    await this.write(
      encodeTsToRustFrame(
        TsToRustMessageTypes.Precompile,
        encodePrecompilePayload({
          code: options.code,
          filename: options.filename,
          limits: options.limits,
          globals: Object.keys(options.globals ?? {}),
        }),
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
    options: { prefixId: string, code: string, filename?: string, limits?: ResourceLimits, globals?: Record<string, HostExportFunction> },
  ): Promise<RawRunResult> {
    if (this.disposed)
      throw new Error('runtime IPC client is disposed')

    const globals = options.globals ?? {}
    const globalNames = Object.keys(globals)
    await this.write(
      encodeTsToRustFrame(
        TsToRustMessageTypes.PrefixRun,
        encodePrefixRunPayload({
          prefixId: options.prefixId,
          code: options.code,
          filename: options.filename,
          limits: options.limits,
          globals: globalNames,
          runId: this.nextRunIdValue(),
        }),
      ),
    )

    return this.drainUntilResult(makeDispatcher(globals))
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

  /**
   * Read frames until the `Result` frame arrives, dispatching `BridgeCall`
   * frames to the handler in the meantime.
   *
   * Dispatches are fire-and-forget — the loop does NOT await the handler.
   * This means the loop continues reading frames immediately. When the Rust
   * side fires a wall timeout it sends a `Result` frame; the loop reads it
   * and returns without waiting for the handler.
   *
   * The handler promise is then **orphaned**: it has no listener and will be
   * garbage-collected when it eventually settles. Any in-flight I/O or timers
   * continue to completion. Rust silently ignores any late `BridgeResponse`
   * that arrives after the run has completed (see session.rs).
   * @param dispatcher
   */
  private async drainUntilResult(
    dispatcher: BridgeCallDispatcher | undefined,
  ): Promise<RawRunResult> {
    for await (const frame of this.reader) {
      switch (frame.messageType) {
        case RustToTsMessageTypes.Result:
          return { result: frame.payload }

        case RustToTsMessageTypes.Log:
          break

        case RustToTsMessageTypes.BridgeCall: {
          const call = decodeBridgeCallPayload(frame.payload)

          if (dispatcher === undefined) {
            // No handler — send a synchronous error response.
            await this.write(
              encodeTsToRustFrame(
                TsToRustMessageTypes.BridgeResponse,
                encodeBridgeResponsePayload(
                  call.callId,
                  false,
                  undefined,
                  'no bridge dispatcher configured',
                ),
              ),
            )
          } else {
            // Fire-and-forget the handler.
            // Do NOT await it — the loop reads the next frame immediately.
            // When the handler settles the response is written back.
            // If the run timed out by then, Rust ignores the late frame.
            const { callId } = call
            dispatcher(call.exportName, call.args).then(
              (value) => {
                // encodeWireValue throws for unrepresentable types (function,
                // symbol, Date, etc.). Catch and send an error response so the
                // sandbox receives ERR_HOST_BRIDGE rather than hanging.
                let encoded: Uint8Array
                try {
                  encoded = encodeWireValue(value)
                } catch (e) {
                  return this.write(
                    encodeTsToRustFrame(
                      TsToRustMessageTypes.BridgeResponse,
                      encodeBridgeResponsePayload(
                        callId,
                        false,
                        undefined,
                        e instanceof Error ? e.message : String(e),
                      ),
                    ),
                  )
                }
                return this.write(
                  encodeTsToRustFrame(
                    TsToRustMessageTypes.BridgeResponse,
                    encodeBridgeResponsePayload(callId, true, encoded),
                  ),
                )
              },
              (err: unknown) => this.write(
                encodeTsToRustFrame(
                  TsToRustMessageTypes.BridgeResponse,
                  encodeBridgeResponsePayload(
                    callId,
                    false,
                    undefined,
                    err instanceof Error ? err.message : String(err),
                  ),
                ),
              ),
            ).catch(() => {
              // Connection may have closed — discard silently.
            })
          }
          break
        }
      }
    }

    throw new Error('connection closed before receiving a Result frame')
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

/**
 * Build a dispatcher from a globals map; returns undefined when map is empty.
 * @param globals
 */
function makeDispatcher(globals: Record<string, HostExportFunction>): BridgeCallDispatcher | undefined {
  const names = Object.keys(globals)
  if (names.length === 0)
    return undefined
  return async (name, args) => {
    const handler = globals[name]
    if (handler === undefined)
      throw new Error(`no handler configured for global '${name}'`)
    return handler(...(args as any[]))
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
