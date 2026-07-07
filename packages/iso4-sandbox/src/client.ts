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
import type { ImportBindingPayload, ResourceLimits } from './ipc'
import { encodeWireValue } from './wire'

export interface RuntimeIpcClientOptions {
  socketPath: string
  token: string
}

export interface RawRunResult {
  result: Uint8Array
}

/**
 * Dispatches a bridge call to the host-configured handler. Resolves the
 * handler by the call's `exportName` against the per-run globals map.
 * Host-module imports route through the same map via a single reserved
 * global, `__iso4_call`, whose handler peels a leading integer handle ID
 * and routes to the import registry (see `imports.ts`).
 */
export type BridgeCallDispatcher = (call: {
  exportName: string
  args: unknown[]
}) => Promise<unknown>

export type { ResourceLimits }

/**
 * Thrown out of a run when its `AbortSignal` fires mid-flight. The connection
 * is torn down (so the Rust isolate is reclaimed) before this propagates, and
 * the client is marked unusable so the pool replaces it rather than reusing a
 * half-dead slot. `index.ts` catches this and synthesizes the `ERR_ABORTED`
 * `RunResult`.
 */
export class RunAbortedError extends Error {
  /** The value passed to `AbortController.abort(reason)`, if any. */
  readonly reason?: unknown
  constructor(reason?: unknown, message = 'run was aborted') {
    super(message)
    this.name = 'RunAbortedError'
    this.reason = reason
  }
}

export class RuntimeIpcClient {
  private readonly socket: Socket
  private readonly reader: FrameReader
  private disposed = false
  /**
   * Set when an in-flight abort tore this connection down. A broken client
   * must not be returned to the pool's free list — `ConnectionPool.release`
   * checks `usable` and replaces it with a fresh connection instead.
   */
  private broken = false

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

  /**
   * A client is usable while its connection is intact. Once an in-flight abort
   * (or a dispose) tears the socket down it becomes unusable and the pool must
   * replace it.
   */
  get usable(): boolean {
    return !this.disposed && !this.broken
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
    options?: {
      filename?: string
      limits?: ResourceLimits
      globals?: Record<string, HostExportFunction>
      imports?: readonly ImportBindingPayload[]
      signal?: AbortSignal
    },
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
          imports: options?.imports,
        }),
      ),
    )

    return this.drainUntilResult(makeDispatcher(globals), options?.signal)
  }

  async precompile(
    options: {
      code: string
      filename?: string
      limits?: ResourceLimits
      globals?: Record<string, HostExportFunction>
      imports?: readonly ImportBindingPayload[]
    },
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
          imports: options.imports,
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
    options: {
      prefixId: string
      code: string
      filename?: string
      limits?: ResourceLimits
      globals?: Record<string, HostExportFunction>
      imports?: readonly ImportBindingPayload[]
      signal?: AbortSignal
    },
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
          imports: options.imports,
          runId: this.nextRunIdValue(),
        }),
      ),
    )

    return this.drainUntilResult(makeDispatcher(globals), options.signal)
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
   *
   * ── In-flight abort ──────────────────────────────────────────────────────
   * When `signal` fires mid-run — including while a bridge call is in flight —
   * we tear the connection down immediately (`abortConnection`): the reader is
   * closed so this loop throws, and the socket is destroyed so the Rust side
   * observes EOF and reclaims the isolate promptly (rather than waiting for
   * `wallTimeMs`). The loop then throws `RunAbortedError`, which `index.ts`
   * maps to an `ERR_ABORTED` `RunResult`. Any late `BridgeResponse` written by
   * an orphaned handler after this point fails silently (the socket is gone),
   * so the sandbox never observes a return value for the in-flight call.
   * @param dispatcher
   * @param signal
   */
  private async drainUntilResult(
    dispatcher: BridgeCallDispatcher | undefined,
    signal?: AbortSignal,
  ): Promise<RawRunResult> {
    // If the signal aborted between the run-entry check in index.ts and here,
    // tear down before we start reading frames.
    if (signal?.aborted) {
      this.abortConnection()
      throw new RunAbortedError(signal.reason)
    }

    const onAbort = (): void => {
      this.abortConnection()
    }
    signal?.addEventListener('abort', onAbort, { once: true })

    try {
      return await this.drainFrames(dispatcher)
    } catch (error) {
      // `abortConnection` closes the reader, which makes the frame loop reject.
      // Translate that (or any error observed once the signal has fired) into a
      // distinguishable abort — carrying the abort reason — so the caller
      // resolves an aborted RunResult.
      if (signal?.aborted)
        throw new RunAbortedError(signal.reason)
      throw error
    } finally {
      signal?.removeEventListener('abort', onAbort)
    }
  }

  /**
   * Tear down the connection in response to an in-flight abort. Idempotent.
   * Marks the client `broken` (so the pool replaces it), closes the frame
   * reader (so `drainFrames` stops awaiting), and destroys the socket (so the
   * Rust session sees EOF and drops the isolate).
   */
  private abortConnection(): void {
    if (this.broken)
      return
    this.broken = true
    this.reader.close(new RunAbortedError())
    this.socket.destroy()
  }

  private async drainFrames(
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
            dispatcher({
              exportName: call.exportName,
              args: call.args,
            }).then(
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
 * Build a dispatcher that looks up a handler by `exportName` in the
 * per-run globals map. Host-module imports are bridged through the same
 * map via the reserved `__iso4_call` global, so a single lookup table
 * handles every bridge call.
 *
 * Returns `undefined` when the map is empty so the loop short-circuits to
 * an "unconfigured bridge" error response.
 * @param globals
 */
function makeDispatcher(
  globals: Record<string, HostExportFunction>,
): BridgeCallDispatcher | undefined {
  if (Object.keys(globals).length === 0)
    return undefined
  return async ({ exportName, args }) => {
    const handler = globals[exportName]
    if (handler === undefined)
      throw new Error(`no handler configured for bridge global '${exportName}'`)
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
