import type { Buffer } from 'node:buffer'
import { createConnection } from 'node:net'
import type { Socket } from 'node:net'
import {
  FrameReader,
  HelloStatus,
  PROTOCOL_VERSION,

  RustToTsMessageTypes,
  TsToRustMessageTypes,
  decodeBridgeCallPayload,
  decodeHelloPayload,
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
import type { HostExportFunction, ResourceLimits } from './types.js'
import type { GlobalDefPayload, ImportBindingPayload, ImportRebindPayload } from './ipc'
import type { ImportHandlerMap } from './imports.js'
import { importHandlerKey } from './imports.js'
import { deserializeValue, serializationProbe, serializeHostValue } from './v8-codec.js'

export interface RuntimeIpcClientOptions {
  socketPath: string
  token: string
}

export interface RawRunResult {
  result: Uint8Array
}

/**
 * Dispatches a bridge call to the host-configured handler. Global calls
 * (`targetKind = 0`) resolve by `exportName` against the per-run globals
 * map; host-import calls (`targetKind = 1`) arrive with their specifier and
 * function-leaf path already resolved by the runtime and route through the
 * per-run import handler map (see `imports.ts`).
 */
export type BridgeCallDispatcher = (call: {
  targetKind: 0 | 1
  specifier: string | undefined
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
 * How long `connect()` waits for the runtime's `Hello` frame before giving up.
 * The runtime answers a handshake without touching V8 (the probe is computed
 * once at process start), so this only bites when the binary is wedged.
 */
const HELLO_TIMEOUT_MS = 5_000

/**
 * Thrown out of `createSandbox()` when the connection handshake fails.
 *
 * The common cause is a V8 serialization format-version mismatch between this
 * Node and the `@iso4/v8-*` binary: values cross the boundary as V8
 * serialization blobs, so the two V8s must agree on the format. Internal —
 * `@iso4/sandbox` does not export this type; it reaches callers as a plain
 * `Error` with `name = 'Iso4HandshakeError'`.
 */
export class HandshakeError extends Error {
  constructor(message: string) {
    super(`[@iso4/sandbox] ${message}`)
    this.name = 'Iso4HandshakeError'
  }
}

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

  /**
   * Open a connection and complete the v2 handshake.
   *
   * Values cross this socket as V8 serialization blobs, so both V8s must agree
   * on the serialization format version. Each side sends a probe (a serialized
   * `null`, whose second byte is the writer's format version) in the handshake
   * and the mismatch is fatal here — at `createSandbox()` time, once per
   * connection — rather than corrupting a value mid-run. The runtime answers
   * with exactly one `Hello` frame; anything else tears the connection down.
   * @param options
   */
  static async connect(options: RuntimeIpcClientOptions): Promise<RuntimeIpcClient> {
    const socket = await connectSocket(options.socketPath)
    const client = new RuntimeIpcClient(socket)

    try {
      await client.write(
        encodeTsToRustFrame(
          TsToRustMessageTypes.Authenticate,
          encodeAuthenticatePayload({
            protocolVersion: PROTOCOL_VERSION,
            probe: serializationProbe(),
            token: options.token,
          }),
        ),
      )
      await client.awaitHello()
    } catch (error) {
      await client.dispose()
      throw error
    }

    return client
  }

  /**
   * Read and validate the runtime's `Hello` frame.
   *
   * Three ways this fails, all fatal and all reported with the same actionable
   * remedy — the host package and the native binary are released in lockstep
   * (`docs/protocol.md` §8), so a mismatch means they are out of sync:
   * the runtime reports a bad status, the frame never arrives, or the
   * runtime's own probe cannot be read by this Node.
   */
  private async awaitHello(): Promise<void> {
    let timer: ReturnType<typeof setTimeout> | undefined
    const frame = await Promise.race([
      this.reader.readRustToTsFrame(),
      new Promise<never>((_resolve, reject) => {
        timer = setTimeout(() => {
          reject(new HandshakeError(
            `V8 runtime did not answer the handshake within ${HELLO_TIMEOUT_MS}ms`,
          ))
        }, HELLO_TIMEOUT_MS)
        timer.unref?.()
      }),
    ]).finally(() => {
      if (timer !== undefined)
        clearTimeout(timer)
    })

    if (frame.messageType !== RustToTsMessageTypes.Hello) {
      throw new HandshakeError(
        `expected a Hello frame from the V8 runtime, got message type 0x${
          frame.messageType.toString(16).padStart(2, '0')
        }`,
      )
    }

    const hello = decodeHelloPayload(frame.payload)
    if (hello.status !== HelloStatus.Ok) {
      throw new HandshakeError(
        hello.message.length > 0
          ? hello.message
          : `V8 runtime rejected the handshake (status ${hello.status})`,
      )
    }

    // Prove empirically — not just from the version byte — that this Node can
    // read what the runtime writes.
    try {
      if (deserializeValue(hello.probe) !== null)
        throw new Error('probe did not decode to null')
    } catch (error) {
      throw new HandshakeError(
        `V8 serialization format mismatch: this Node cannot read values written by the `
        + `iso4-v8 binary (${error instanceof Error ? error.message : String(error)}). `
        + `Update @iso4/sandbox and @iso4/v8-* together — they are released in lockstep.`,
      )
    }
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

  // `globals` is the wire-shaped `GlobalDefPayload[]` produced by
  // `processGlobals()` in index.ts — Rust installs each kind natively.
  // `dispatch` is the separate `name → handler` map the client routes incoming
  // global `BridgeCall` frames through: plain functions under their own name,
  // `BridgeWithShim` handlers under their private `__iso4_<name>_h` key.
  // `importDispatch` routes import-targeted frames by (specifier, leaf path).
  async runRawCode(
    code: string,
    options?: {
      filename?: string
      limits?: ResourceLimits
      globals?: readonly GlobalDefPayload[]
      dispatch?: Record<string, HostExportFunction>
      imports?: readonly ImportBindingPayload[]
      importDispatch?: ImportHandlerMap
      signal?: AbortSignal
    },
  ): Promise<RawRunResult> {
    if (this.disposed)
      throw new Error('runtime IPC client is disposed')

    const runId = this.nextRunIdValue()
    await this.write(
      encodeTsToRustFrame(
        TsToRustMessageTypes.Run,
        encodeRunPayload({
          runId,
          code,
          filename: options?.filename,
          limits: options?.limits,
          globals: options?.globals,
          imports: options?.imports,
        }),
      ),
    )

    return this.drainUntilResult(
      makeDispatcher(options?.dispatch ?? {}, options?.importDispatch),
      runId,
      options?.signal,
    )
  }

  async precompile(
    options: {
      code: string
      filename?: string
      limits?: ResourceLimits
      globals?: readonly GlobalDefPayload[]
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
          globals: options.globals,
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
      globals?: readonly GlobalDefPayload[]
      dispatch?: Record<string, HostExportFunction>
      importRebinds?: readonly ImportRebindPayload[]
      importDispatch?: ImportHandlerMap
      signal?: AbortSignal
    },
  ): Promise<RawRunResult> {
    if (this.disposed)
      throw new Error('runtime IPC client is disposed')

    const runId = this.nextRunIdValue()
    await this.write(
      encodeTsToRustFrame(
        TsToRustMessageTypes.PrefixRun,
        encodePrefixRunPayload({
          prefixId: options.prefixId,
          code: options.code,
          filename: options.filename,
          limits: options.limits,
          globals: options.globals,
          importRebinds: options.importRebinds,
          runId,
        }),
      ),
    )

    return this.drainUntilResult(
      makeDispatcher(options.dispatch ?? {}, options.importDispatch),
      runId,
      options.signal,
    )
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
              targetKind: call.targetKind,
              specifier: call.specifier,
              exportName: call.exportName,
              args: call.args,
            }).then(
              async (value) => {
                // serializeHostValue throws for the handful of types V8 refuses
                // to clone (function, symbol, promise, WeakMap, proxy). Catch
                // and send an error response so the sandbox receives
                // ERR_HOST_BRIDGE rather than hanging.
                //
                // The async variant is used here because a handler may return a
                // Request/Response — a `fetch` handler returning the real thing
                // rather than a plain object — and draining its body is async.
                let encoded: Uint8Array
                try {
                  encoded = await serializeHostValue(value)
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
 * Build a dispatcher over the two per-run handler maps: globals by
 * `exportName`, host-import function leaves by `(specifier, leaf path)` —
 * the runtime resolves import handle IDs before the frame is sent, so both
 * lookups are plain name-addressed.
 *
 * Returns `undefined` when both maps are empty so the loop short-circuits to
 * an "unconfigured bridge" error response.
 * @param globals
 * @param imports
 */
function makeDispatcher(
  globals: Record<string, HostExportFunction>,
  imports?: ImportHandlerMap,
): BridgeCallDispatcher | undefined {
  if (Object.keys(globals).length === 0 && (imports === undefined || imports.size === 0))
    return undefined
  return async ({ targetKind, specifier, exportName, args }) => {
    if (targetKind === 1) {
      const handler = imports?.get(importHandlerKey(specifier ?? '', exportName))
      if (handler === undefined) {
        throw new Error(
          `no handler configured for host import '${specifier}'.${exportName}`,
        )
      }
      return handler(...(args as any[]))
    }
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
