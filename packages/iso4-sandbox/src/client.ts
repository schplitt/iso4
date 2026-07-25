import type { Buffer } from 'node:buffer'
import { createConnection } from 'node:net'
import type { Socket } from 'node:net'
import {
  FrameReader,
  PROTOCOL_VERSION,

  RustToTsMessageTypes,
  TsToRustMessageTypes,
  decodeBridgeCallPayload,
  bridgeErrorPayloadFromUnknown,
  encodeBridgeResponsePayload,
  encodeAuthenticatePayload,
  encodeDisposePrefixPayload,
  encodePrecompilePayload,
  encodePrefixRunPayload,
  encodeRunPayload,
  encodeTerminatePayload,
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
 * How long TS waits for Rust's graceful `ERR_ABORTED` Result after sending a
 * `Terminate` frame, before falling back to tearing the connection down (#36).
 *
 * The common case — a run suspended awaiting a bridge response — has the Rust
 * V8 thread parked on a socket read, so it consumes the `Terminate` and replies
 * in well under a millisecond; this window only bites when the sandbox is stuck
 * in a tight synchronous loop (Rust never reads the frame), where we fall back.
 * Kept comfortably under the abort-latency the runtime aims for.
 */
const TERMINATE_GRACE_MS = 100

/**
 * Thrown out of a run when its `AbortSignal` fires mid-flight and the graceful
 * `Terminate` path did not produce a Result within {@link TERMINATE_GRACE_MS}
 * (or when the pre-run abort race tears down before draining). The connection
 * is torn down (so the Rust isolate is reclaimed) before this propagates, and
 * the client is marked unusable so the pool replaces it rather than reusing a
 * half-dead slot. `index.ts` catches this and synthesizes the `ERR_ABORTED`
 * `RunResult`. When the graceful path succeeds instead, no error is thrown —
 * the real `ERR_ABORTED` Result flows back through `drainFrames`.
 */
export class RunAbortedError extends Error {
  /**
   * The value passed to `AbortController.abort(reason)`, if any.
   */
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
    const runId = this.nextRunIdValue()
    await this.write(
      encodeTsToRustFrame(
        TsToRustMessageTypes.Run,
        encodeRunPayload({
          runId,
          code,
          filename: options?.filename,
          limits: options?.limits,
          globals: globalNames,
          imports: options?.imports,
        }),
      ),
    )

    return this.drainUntilResult(makeDispatcher(globals), runId, options?.signal)
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
    const runId = this.nextRunIdValue()
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
          runId,
        }),
      ),
    )

    return this.drainUntilResult(makeDispatcher(globals), runId, options.signal)
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
   * ── In-flight abort (graceful terminate, #36) ─────────────────────────────
   * When `signal` fires mid-run — including while a bridge call is in flight —
   * we first ask Rust to stop gracefully: send a `Terminate` frame (carrying
   * `runId`) and keep draining, leaving the socket open. In the common case the
   * Rust V8 thread is parked awaiting a bridge response, reads the frame, and
   * replies with a real `ERR_ABORTED` `Result` (carrying duration, CPU time,
   * and the bridge records collected so far). That Result flows back through
   * `drainFrames` and the connection stays healthy for reuse.
   *
   * If no Result arrives within {@link TERMINATE_GRACE_MS} — the sandbox is
   * stuck in a tight synchronous loop, so Rust never reaches the frame read —
   * we fall back to `abortConnection`: the reader is closed so this loop
   * throws, and the socket is destroyed so Rust observes EOF (its CPU guard
   * ultimately reclaims the busy isolate; see DESIGN.md §14.7). The loop then
   * throws `RunAbortedError`, which `index.ts` maps to a synthesized
   * `ERR_ABORTED` `RunResult`. Any late `BridgeResponse` from an orphaned
   * handler is harmless either way: on the graceful path the reused connection
   * discards it (stale callId), on the fallback path the socket is gone.
   * @param dispatcher
   * @param runId
   * @param signal
   */
  private async drainUntilResult(
    dispatcher: BridgeCallDispatcher | undefined,
    runId: number,
    signal?: AbortSignal,
  ): Promise<RawRunResult> {
    // If the signal aborted between the run-entry check in index.ts and here,
    // tear down before we start reading frames. The Run frame is already on the
    // wire but the run has barely started (and may have no bridge poll loop to
    // read a Terminate), so the graceful path cannot reliably apply here.
    if (signal?.aborted) {
      this.abortConnection()
      throw new RunAbortedError(signal.reason)
    }

    let graceTimer: ReturnType<typeof setTimeout> | undefined
    const onAbort = (): void => {
      // Ask Rust to stop and send a real ERR_ABORTED Result. Fire-and-forget:
      // if the write fails the socket is already broken, and the fallback timer
      // (or a reader error) resolves the run anyway.
      this.write(
        encodeTsToRustFrame(
          TsToRustMessageTypes.Terminate,
          encodeTerminatePayload(runId),
        ),
      ).catch(() => {
        // Socket already gone — nothing to gracefully terminate.
      })
      graceTimer = setTimeout(() => {
        this.abortConnection()
      }, TERMINATE_GRACE_MS)
      // Don't let the grace timer alone keep the event loop alive.
      graceTimer.unref?.()
    }
    signal?.addEventListener('abort', onAbort, { once: true })

    try {
      return await this.drainFrames(dispatcher)
    } catch (error) {
      // The fallback `abortConnection` closes the reader, which makes the frame
      // loop reject. Translate that (or any error observed once the signal has
      // fired) into a distinguishable abort — carrying the abort reason — so the
      // caller resolves an aborted RunResult.
      if (signal?.aborted)
        throw new RunAbortedError(signal.reason)
      throw error
    } finally {
      if (graceTimer !== undefined)
        clearTimeout(graceTimer)
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
                  { name: 'Error', message: 'no bridge dispatcher configured' },
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
                        {
                          name: 'Error',
                          message: e instanceof Error ? e.message : String(e),
                        },
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
                    bridgeErrorPayloadFromUnknown(err),
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
