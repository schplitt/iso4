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
  decodeStatsPayload,
  bridgeErrorPayloadFromUnknown,
  encodeBridgeResponsePayload,
  encodeAuthenticatePayload,
  encodeDisposePrefixPayload,
  encodePrecompilePayload,
  encodePrefixRunPayload,
  encodeRunPayload,
  encodeTerminatePayload,
  encodeTsToRustFrame,
  decodeRunCompletePayload,
  peekBridgeCallId,
  peekBridgeCallRunId,
  peekPrecompileResultRequestId,
  peekRunCompletionBackgroundPending,
  peekRunCompletionRunId,
  STREAM_CHUNK_MAX_BYTES,
  STREAM_CREDIT_WINDOW_BYTES,
  decodeStreamCancelPayload,
  decodeStreamPullPayload,
  encodeStreamChunkPayload,
  encodeStreamEndPayload,
} from './ipc'
import type { WireResourceLimits, CallPayload, DecodedRunComplete, GlobalDefPayload, ImportBindingPayload, ImportRebindPayload, RustToTsFrame, RuntimeStatsPayload } from './ipc'
import type { HostExportFunction, ResourceLimits } from './types.js'
import type { ImportHandlerMap } from './imports.js'
import { importHandlerKey } from './imports.js'
import { deserializeValue, serializationProbe, serializeHostValue } from './v8-codec.js'
import type { StreamSourceRegistry } from './web-codec.js'
import { brandKeyForToken } from './web-codec.js'

export interface RuntimeIpcClientOptions {
  socketPath: string
  /**
   * The sandbox's random descriptor token (16 bytes), sent in the
   * `Authenticate` frame. The runtime rehydrates only host-type descriptors
   * stamped with the brand key derived from it — see `web-codec.ts`. One token
   * per sandbox, shared by every pooled connection.
   */
  descriptorToken: Uint8Array
}

export interface RawRunResult {
  result: Uint8Array
  /**
   * Present when the Result reported pending `waitUntil` work: the run
   * continues runtime-side and this settles when its `RunComplete` frame
   * arrives — `undefined` on connection loss. The run's slot is already
   * free; the grace frames ride the shared connection, routed by run id.
   */
  epilogue?: Promise<DecodedRunComplete | undefined>
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
 * `Terminate` frame, before falling back to tearing the connection down.
 *
 * The runtime's session demux always consumes the frame: a suspended run is
 * abandoned on the spot, and a CPU-bound run is terminated mid-execution —
 * both answer with a real Result in well under this window. The fallback
 * only bites when the runtime cannot answer at all (wedged process).
 */
const TERMINATE_GRACE_MS = 100

/**
 * How long `connect()` waits for the runtime's `Hello` frame before giving up.
 * The runtime answers a handshake without touching V8 (the probe is computed
 * once at process start), so this only bites when the binary is wedged.
 */
const HELLO_TIMEOUT_MS = 5_000

/**
 * Shared placeholder for `RunEntry` callbacks between wiring phases —
 * allocating fresh `() => {}` closures per run is avoidable hot-path churn.
 */
function NOOP(): void {}

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
 * (or when the pre-run abort race tears down before the run settles). The
 * connection is torn down (so the Rust isolate is reclaimed) before this
 * propagates, and the client is marked unusable so the pool replaces it rather
 * than reusing a half-dead slot. `index.ts` catches this and synthesizes the
 * `ERR_ABORTED` `RunResult`. When the graceful path succeeds instead, no error
 * is thrown — the real `ERR_ABORTED` Result flows back through the router.
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

/**
 * Thrown when a frame arrives that cannot belong to any run in flight on this
 * connection: a `Result` or `RunComplete` carrying a run id nothing here is
 * waiting on, or a frame type with no place in the protocol. Either way the
 * two sides have lost agreement about what this connection is doing, so
 * nothing it delivers from now on can be attributed with confidence.
 *
 * The connection is torn down before this propagates — every run in flight on
 * it fails with this error — because a peer that is mid-run on someone else's
 * frames must never be handed to the next caller. `index.ts` catches this and
 * synthesizes the `ERR_PROTOCOL_DESYNC` `RunResult` — a displaced run never
 * executed, so there is no partial telemetry to report.
 */
export class ProtocolDesyncError extends Error {
  constructor(message: string) {
    super(`[@iso4/sandbox] ${message}`)
    this.name = 'Iso4ProtocolDesyncError'
  }
}

/**
 * Per-run routing state, keyed by `runId` in {@link RuntimeIpcClient.runs}.
 * One entry exists from the moment the run's request frame is written until
 * its final frame (`Result`, or `RunComplete` when `waitUntil` work ran)
 * settles it — the router consults nothing else, so several runs can be in
 * flight on one connection and every frame finds its owner by run id alone.
 */
interface RunEntry {
  dispatcher: BridgeCallDispatcher | undefined
  streams: StreamSourceRegistry | undefined
  signal: AbortSignal | undefined
  resolve: (result: RawRunResult) => void
  reject: (error: Error) => void
  /**
   * Present once the run's Result reported pending `waitUntil` work: settles
   * the epilogue promise handed to the caller. Its presence also marks the
   * entry as grace-phase — a second `Result` for it is desync, and teardown
   * resolves it `undefined` instead of rejecting (the run's value was already
   * delivered, so nothing user-facing is allowed to fail).
   */
  epilogue?: (report: DecodedRunComplete | undefined) => void
  /**
   * Removes the abort wiring installed for the current phase (mid-run:
   * Terminate + teardown fallback timer; grace phase: Terminate only).
   * Called at every settle point, including connection teardown.
   */
  detachAbort: () => void
}

/**
 * A pending `precompile()` or `stats()` request: both are answered by exactly
 * one payload-only frame with no run id, so the router matches replies to
 * requests in FIFO order per expected frame type.
 */
interface ControlWaiter {
  expects: number
  resolve: (payload: Uint8Array) => void
  reject: (error: Error) => void
}

export class RuntimeIpcClient {
  private readonly socket: Socket
  private readonly reader: FrameReader
  private disposed = false
  /**
   * Set when this connection was torn down (abort fallback, desync, peer
   * close). A broken client must not be returned to the pool's idle list —
   * the connection registry checks `usable` and drops it instead.
   */
  private broken = false

  /**
   * Runs in flight on this connection, keyed by run id — the router's only
   * routing table. Several runs share one connection; every frame finds its
   * owner by run id alone.
   */
  private readonly runs = new Map<number, RunEntry>()

  /**
   * Everything routed on this connection that a connection-level failure
   * would take down: runs (executing AND grace-phase — an entry lives until
   * its final frame) plus in-flight control requests. The pool's
   * per-connection cap reads this, so the blast-radius bound covers all of
   * it. Both counts move synchronously from the call that adds them
   * (`runRawCode`/`prefixRun`/`precompile`), which keeps the pool's
   * same-tick load observation exact.
   */
  get load(): number {
    return this.runs.size + this.precompileWaiters.size + this.controlWaiters.length
  }

  /**
   * Pending `stats()` requests, FIFO — sound because the runtime answers
   * `Stats` inline in its demux loop, so replies come back in request
   * order. See {@link ControlWaiter}.
   */
  private readonly controlWaiters: ControlWaiter[] = []

  /**
   * Pending `precompile()` requests keyed by their wire `requestId`.
   * Precompiles are answered on runtime worker threads, so with several in
   * flight on one connection the replies can return out of request order —
   * FIFO matching would cross-assign prefix ids between callers.
   */
  private readonly precompileWaiters = new Map<number, {
    resolve: (payload: Uint8Array) => void
    reject: (error: Error) => void
  }>()

  /**
   * Session brand key for host-type descriptors written on this connection.
   */
  private readonly brandKey: string

  private constructor(socket: Socket, brandKey: string) {
    this.socket = socket
    this.brandKey = brandKey
    this.reader = new FrameReader()
    socket.on('data', (chunk: Buffer) => {
      this.reader.push(chunk)
    })
    // Every one of these is a connection that cannot serve another run, so each
    // marks the client broken as well as closing the reader — the router loop
    // observes the close and fails everything in flight. Without the flag the
    // pool reads `usable === true` off a dead socket and recycles it forever
    // (the peer-close path never goes through `abortConnection`).
    socket.once('error', (error: Error) => {
      this.broken = true
      this.reader.close(error)
    })
    socket.once('end', () => {
      this.broken = true
      this.reader.close(new Error('socket ended'))
    })
    socket.once('close', () => {
      this.broken = true
      this.reader.close(new Error('socket closed'))
    })
  }

  /**
   * A client is usable while its connection is intact. Once an in-flight abort
   * (or a dispose, or a desync) tears the socket down it becomes unusable and
   * the pool must replace it.
   *
   * The socket is consulted directly as well as the `broken` flag: a socket can
   * be destroyed without either handler above having run yet (a synchronous
   * `destroy()` earlier in the same tick), and a recycled dead connection fails
   * every future run drawn from that slot.
   */
  get usable(): boolean {
    return (
      !this.disposed
      && !this.broken
      && !this.socket.destroyed
      && !this.socket.closed
    )
  }

  /**
   * Open a connection and complete the handshake.
   *
   * Values cross this socket as V8 serialization blobs, so both V8s must agree
   * on the serialization format version. Each side sends a probe (a serialized
   * `null`, whose second byte is the writer's format version) in the handshake
   * and the mismatch is fatal here — once per connection — rather than
   * corrupting a value mid-run. The runtime answers with exactly one `Hello`
   * frame; anything else tears the connection down. Once the handshake is
   * done the connection's frame router starts and owns the reader for the
   * connection's lifetime.
   * @param options
   */
  static async connect(options: RuntimeIpcClientOptions): Promise<RuntimeIpcClient> {
    const socket = await connectSocket(options.socketPath)
    const client = new RuntimeIpcClient(socket, brandKeyForToken(options.descriptorToken))

    try {
      await client.write(
        encodeTsToRustFrame(
          TsToRustMessageTypes.Authenticate,
          encodeAuthenticatePayload({
            protocolVersion: PROTOCOL_VERSION,
            probe: serializationProbe(),
            descriptorToken: options.descriptorToken,
          }),
        ),
      )
      await client.awaitHello()
    } catch (error) {
      await client.dispose()
      throw error
    }

    client.routeFrames()
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
    // Wraps at 2³²-1 back to 0. `>>> 0` and not `& 0xffffffff`: `&` coerces
    // through ToInt32, so `(0x7fffffff + 1) & 0xffffffff` is -2147483648, which
    // `writeU32` then rejects with ERR_OUT_OF_RANGE — killing every subsequent
    // run on the connection for the next 2³¹ increments.
    //
    // Run id 0 is skipped (it disables stream-frame run validation on both
    // sides), and so is any id still routing in `runs` — after a wrap, a
    // rolled-over id colliding with a live run would splice two runs' frames
    // together.
    do {
      this.nextRunId = (this.nextRunId + 1) >>> 0
    } while (this.nextRunId === 0 || this.runs.has(this.nextRunId))
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
      limits?: WireResourceLimits
      globals?: readonly GlobalDefPayload[]
      dispatch?: Record<string, HostExportFunction>
      imports?: readonly ImportBindingPayload[]
      importDispatch?: ImportHandlerMap
      signal?: AbortSignal
      call?: CallPayload
      streams?: StreamSourceRegistry
    },
  ): Promise<RawRunResult> {
    if (this.disposed)
      throw new Error('runtime IPC client is disposed')

    const runId = this.nextRunIdValue()
    return this.executeRun(
      runId,
      encodeTsToRustFrame(
        TsToRustMessageTypes.Run,
        encodeRunPayload({
          runId,
          code,
          filename: options?.filename,
          limits: options?.limits,
          globals: options?.globals,
          imports: options?.imports,
          call: options?.call,
        }),
      ),
      makeDispatcher(options?.dispatch ?? {}, options?.importDispatch),
      options?.signal,
      options?.streams,
    )
  }

  async prefixRun(
    options: {
      prefixId: string
      /**
       * Postfix source, or absent for a call-only run — exactly one of
       * `code` / `call` must be present.
       */
      code?: string
      filename?: string
      limits?: WireResourceLimits
      globals?: readonly GlobalDefPayload[]
      dispatch?: Record<string, HostExportFunction>
      importRebinds?: readonly ImportRebindPayload[]
      importDispatch?: ImportHandlerMap
      signal?: AbortSignal
      call?: CallPayload
      streams?: StreamSourceRegistry
    },
  ): Promise<RawRunResult> {
    if (this.disposed)
      throw new Error('runtime IPC client is disposed')

    const runId = this.nextRunIdValue()
    return this.executeRun(
      runId,
      encodeTsToRustFrame(
        TsToRustMessageTypes.PrefixRun,
        encodePrefixRunPayload({
          prefixId: options.prefixId,
          code: options.code,
          filename: options.filename,
          limits: options.limits,
          globals: options.globals,
          importRebinds: options.importRebinds,
          call: options.call,
          runId,
        }),
      ),
      makeDispatcher(options.dispatch ?? {}, options.importDispatch),
      options.signal,
      options.streams,
    )
  }

  /**
   * Shared body of `runRawCode` and `prefixRun`: register the run with the
   * router, write its request frame, start its stream pumps, and await its
   * settlement. Registration happens *before* the write so the router can
   * never see a frame for a run it does not know yet.
   *
   * ── In-flight abort (graceful terminate) ──────────────────────────────────
   * When `signal` fires mid-run — including while a bridge call is in flight —
   * we first ask Rust to stop gracefully: send a `Terminate` frame (carrying
   * `runId`) and keep the connection routing. The runtime's demux consumes the
   * frame and either abandons a suspended run or terminates a CPU-bound one
   * mid-execution; both reply with a real `ERR_ABORTED` `Result` (carrying
   * duration, CPU time, and the bridge records collected so far). That Result
   * flows back through the router and the connection stays healthy for reuse.
   *
   * If no Result arrives within {@link TERMINATE_GRACE_MS} — the runtime
   * cannot answer at all (wedged process) — we fall back to
   * `abortConnection`: the reader is closed so the router fails every run in
   * flight, and the socket is destroyed so Rust observes EOF (see DESIGN.md
   * §14.7). This run then rejects `RunAbortedError`, which `index.ts` maps to
   * a synthesized `ERR_ABORTED` `RunResult`. Any late `BridgeResponse` from an
   * orphaned handler is harmless either way: on the graceful path the routed
   * connection discards it runtime-side (stale callId), on the fallback path
   * the socket is gone.
   * @param runId
   * @param frame the encoded `Run`/`PrefixRun` frame
   * @param dispatcher
   * @param signal
   * @param streams
   */
  private async executeRun(
    runId: number,
    frame: Buffer,
    dispatcher: BridgeCallDispatcher | undefined,
    signal?: AbortSignal,
    streams?: StreamSourceRegistry,
  ): Promise<RawRunResult> {
    const entry: RunEntry = {
      dispatcher,
      streams,
      signal,
      resolve: NOOP,
      reject: NOOP,
      detachAbort: NOOP,
    }
    const settled = new Promise<RawRunResult>((resolve, reject) => {
      entry.resolve = resolve
      entry.reject = reject
    })
    // A teardown can reject `settled` during the request write below, before
    // the `await settled` handler is attached — keep that window from
    // surfacing as an unhandled rejection (the real await still observes it).
    settled.catch(() => {})
    let graceTimer: ReturnType<typeof setTimeout> | undefined
    // Ask Rust to stop and send a real ERR_ABORTED Result. Fire-and-forget:
    // if the write fails the socket is already broken, and the fallback
    // timer (or a reader error) resolves the run anyway. The fallback tears
    // the whole connection down — with several runs multiplexed on it that
    // is a documented blast radius, acceptable because the runtime answers
    // a Terminate in well under the window unless the child is wedged, in
    // which case every run on the connection is already lost.
    const beginGracefulAbort = (): void => {
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
    if (signal !== undefined && !signal.aborted) {
      signal.addEventListener('abort', beginGracefulAbort, { once: true })
      entry.detachAbort = () => {
        signal.removeEventListener('abort', beginGracefulAbort)
        if (graceTimer !== undefined)
          clearTimeout(graceTimer)
      }
    }
    this.runs.set(runId, entry)

    try {
      await this.write(frame)
      // The frame carrying stream handles is on the wire; start their pumps.
      this.activateStreams(streams, runId)
      // If the signal aborted between the run-entry check in index.ts and
      // here, the listener above never fired (it was attached to an
      // un-aborted signal, or never attached): take the same graceful path.
      // The runtime registers the run's route before dispatching it, so a
      // Terminate sent right behind the request frame lands — tearing the
      // whole connection down for this benign race would cost every
      // co-resident run.
      if (signal?.aborted && graceTimer === undefined) {
        beginGracefulAbort()
        entry.detachAbort = () => {
          if (graceTimer !== undefined)
            clearTimeout(graceTimer)
        }
      }
    } catch (error) {
      if (this.runs.delete(runId)) {
        entry.detachAbort()
        entry.reject(error instanceof Error ? error : new Error(String(error)))
      }
    }

    try {
      return await settled
    } catch (error) {
      // Teardown rejections arrive here as whatever closed the connection.
      // Translate any error observed once the signal has fired into a
      // distinguishable abort — carrying the abort reason — so the caller
      // resolves an aborted RunResult.
      if (signal?.aborted)
        throw new RunAbortedError(signal.reason)
      throw error
    }
  }

  private nextPrecompileId = 0
  private nextPrecompileIdValue(): number {
    // Same discipline as run ids: wrap unsigned, skip any id still in
    // flight so a reply can never be matched to the wrong caller.
    do {
      this.nextPrecompileId = (this.nextPrecompileId + 1) >>> 0
    } while (this.precompileWaiters.has(this.nextPrecompileId))
    return this.nextPrecompileId
  }

  async precompile(
    options: {
      code: string
      filename?: string
      limits?: WireResourceLimits
      globals?: readonly GlobalDefPayload[]
      imports?: readonly ImportBindingPayload[]
    },
  ): Promise<Uint8Array> {
    if (this.disposed)
      throw new Error('runtime IPC client is disposed')

    const requestId = this.nextPrecompileIdValue()
    const frame = encodeTsToRustFrame(
      TsToRustMessageTypes.Precompile,
      encodePrecompilePayload({
        requestId,
        code: options.code,
        filename: options.filename,
        limits: options.limits,
        globals: options.globals,
        imports: options.imports,
      }),
    )
    const reply = new Promise<Uint8Array>((resolve, reject) => {
      this.precompileWaiters.set(requestId, { resolve, reject })
    })
    // A teardown can reject the waiter while the request write below is
    // still awaited — keep that window from surfacing as an unhandled
    // rejection (the caller's await still observes it).
    reply.catch(NOOP)
    try {
      await this.write(frame)
    } catch (error) {
      this.precompileWaiters.delete(requestId)
      throw error
    }
    return reply
  }

  /**
   * Request the runtime's capacity/usage snapshot. Empty request
   * payload; the reply is one `StatsResult` frame. Sent on the sandbox's
   * dedicated control connection so it never queues behind runs.
   */
  async stats(): Promise<RuntimeStatsPayload> {
    if (this.disposed)
      throw new Error('runtime IPC client is disposed')

    const payload = await this.controlRequest(
      encodeTsToRustFrame(TsToRustMessageTypes.Stats, new Uint8Array(0)),
      RustToTsMessageTypes.StatsResult,
    )
    return decodeStatsPayload(payload)
  }

  /**
   * Write a request answered by exactly one payload-only reply frame
   * (`Precompile` → `PrecompileResult`, `Stats` → `StatsResult`) and await
   * that reply through the router. Replies carry no correlation id, so the
   * router matches them FIFO per expected frame type — a reply nobody is
   * waiting on is a desync, the same contract the run path enforces.
   * @param frame the encoded request frame
   * @param expects the reply frame type
   */
  private async controlRequest(frame: Buffer, expects: number): Promise<Uint8Array> {
    let waiter!: ControlWaiter
    const reply = new Promise<Uint8Array>((resolve, reject) => {
      waiter = { expects, resolve, reject }
    })
    // A teardown can reject the waiter while the request write below is
    // still awaited — keep that window from surfacing as an unhandled
    // rejection (the caller's await still observes it).
    reply.catch(() => {})
    this.controlWaiters.push(waiter)
    try {
      await this.write(frame)
    } catch (error) {
      const at = this.controlWaiters.indexOf(waiter)
      if (at !== -1)
        this.controlWaiters.splice(at, 1)
      throw error
    }
    return reply
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
   * Tear down the connection in response to an in-flight abort the runtime
   * did not answer. Idempotent. Marks the client `broken` (so the pool
   * replaces it), closes the frame reader (which stops the router — it fails
   * every run and control request still in flight), and destroys the socket
   * (so the Rust session sees EOF and drops its isolates).
   */
  private abortConnection(): void {
    if (this.broken)
      return
    this.broken = true
    this.reader.close(new Error(
      '[@iso4/sandbox] connection torn down: an aborted run got no answer from the runtime',
    ))
    this.socket.destroy()
  }

  /**
   * The connection's frame router — the single consumer of the frame reader
   * for the connection's lifetime, started right after the handshake. Every
   * run-tagged frame is routed to its run's entry by the run id it leads
   * with; `Result`/`RunComplete` frames for a run id nothing here is waiting
   * on are the desync signal ({@link ProtocolDesyncError}), while late
   * `BridgeCall`/stream frames for an already-completed run are discarded the
   * way the runtime discards late `BridgeResponse`s.
   *
   * The loop ends only when the reader closes (peer EOF, socket error,
   * dispose, abort fallback) or a routing error is thrown (desync, a decode
   * throw on guest-controlled bytes that cannot be answered inline). Either
   * way the connection is done: framing or attribution is no longer
   * trustworthy, so everything in flight fails now rather than waiting on
   * frames that will never be delivered — and the connection never goes back
   * to the pool.
   */
  private routeFrames(): void {
    (async () => {
      try {
        for await (const frame of this.reader) {
          // routeFrame is synchronous for every arm except a BridgeCall that
          // needs an inline response write — await only when it says so, so
          // the steady-state per-frame cost is a switch and a Map lookup,
          // not a promise (the frame rate scales with multiplexing).
          const pending = this.routeFrame(frame)
          if (pending !== undefined)
            await pending
        }
      } catch (error) {
        this.broken = true
        const failure = error instanceof Error ? error : new Error(String(error))
        this.reader.close(failure)
        this.socket.destroy()
        for (const [, entry] of this.runs) {
          entry.detachAbort()
          // Nothing will pump these sources anymore. Grace-phase runs need
          // this here: their caller already resolved at the Result, so no
          // caller-side catch releases the registry, and an idle source
          // would keep its host ReadableStream locked forever.
          entry.streams?.releaseAll()
          if (entry.epilogue !== undefined) {
            // The run's value was already delivered; the caller synthesizes a
            // truncated waitUntil report from `undefined`.
            entry.epilogue(undefined)
          } else {
            entry.reject(failure)
          }
        }
        this.runs.clear()
        for (const waiter of this.controlWaiters.splice(0))
          waiter.reject(failure)
        for (const [, waiter] of this.precompileWaiters)
          waiter.reject(failure)
        this.precompileWaiters.clear()
      }
    })()
  }

  /**
   * Dispatch one frame to its owner. Throws to end the connection (the
   * router's catch fails everything in flight). Synchronous except for a
   * BridgeCall that writes an inline response — that arm returns a promise
   * for the loop to await; everything else returns `undefined` so routing a
   * frame costs no promise.
   * @param frame
   */
  private routeFrame(frame: RustToTsFrame): Promise<void> | undefined {
    switch (frame.messageType) {
      case RustToTsMessageTypes.Result:
        this.routeResult(frame.payload)
        return undefined

      case RustToTsMessageTypes.RunComplete:
        this.routeRunComplete(frame.payload)
        return undefined

      case RustToTsMessageTypes.Log:
        return undefined

      case RustToTsMessageTypes.PrecompileResult: {
        // Routed by the echoed requestId: precompile replies can return out
        // of request order (worker threads runtime-side), so FIFO matching
        // would cross-assign prefix ids between concurrent prepare() calls.
        const requestId = peekPrecompileResultRequestId(frame.payload)
        const waiter = requestId === undefined
          ? undefined
          : this.precompileWaiters.get(requestId)
        if (requestId === undefined || waiter === undefined) {
          throw new ProtocolDesyncError(
            `PrecompileResult carries requestId ${requestId ?? '(truncated payload)'} `
            + 'but no precompile with that id is pending on this connection',
          )
        }
        this.precompileWaiters.delete(requestId)
        waiter.resolve(frame.payload)
        return undefined
      }

      case RustToTsMessageTypes.StatsResult: {
        const waiter = this.controlWaiters[0]
        if (waiter === undefined || waiter.expects !== frame.messageType) {
          throw new ProtocolDesyncError(
            `unexpected frame type 0x${frame.messageType.toString(16).padStart(2, '0')} `
            + 'with no matching request pending on this connection',
          )
        }
        this.controlWaiters.shift()
        waiter.resolve(frame.payload)
        return undefined
      }

      case RustToTsMessageTypes.BridgeCall: {
        const runId = peekBridgeCallRunId(frame.payload)
        if (runId === undefined) {
          throw new ProtocolDesyncError(
            'BridgeCall payload too short to carry a run id',
          )
        }
        const entry = this.runs.get(runId)
        // No owner: the run completed while the frame was in flight. Discard,
        // exactly as the runtime discards late BridgeResponses — the run is
        // gone on both sides, so nobody is parked on an answer.
        if (entry === undefined)
          return undefined
        return this.dispatchBridgeCallFrame(frame.payload, entry, runId)
      }

      case RustToTsMessageTypes.StreamPull: {
        const pull = decodeStreamPullPayload(frame.payload)
        const owner = this.streamOwner(pull.runId)
        if (owner === undefined)
          return undefined
        const source = owner.streams.sources.get(pull.streamId)
        if (source === undefined)
          return undefined
        source.credit += pull.credit
        this.pumpStream(owner.streams, owner.runId, pull.streamId)
        return undefined
      }

      case RustToTsMessageTypes.StreamCancel: {
        const cancel = decodeStreamCancelPayload(frame.payload)
        const owner = this.streamOwner(cancel.runId)
        if (owner === undefined)
          return undefined
        const source = owner.streams.sources.get(cancel.streamId)
        if (source === undefined)
          return undefined
        source.done = true
        source.reader.cancel(cancel.reason).catch(() => {})
        owner.streams.sources.delete(cancel.streamId)
        return undefined
      }

      default:
        // Only the types above are legal after the handshake. Anything else
        // means the two sides desynced — fail loudly rather than skipping the
        // frame and reading on past it.
        throw new ProtocolDesyncError(
          `unexpected frame type 0x${frame.messageType.toString(16).padStart(2, '0')}`,
        )
    }
  }

  /**
   * Route a `Result` frame: Rust echoes the `runId` of the payload it is
   * answering onto every completion path (`session.rs` →
   * `wire::encode_run_completion_payload`), so a `Result` for a run id nothing
   * here is waiting on is proof the two sides disagree about what this
   * connection is doing.
   * @param payload
   */
  private routeResult(payload: Uint8Array): void {
    const runId = peekRunCompletionRunId(payload)
    const entry = runId === undefined ? undefined : this.runs.get(runId)
    if (runId === undefined || entry === undefined || entry.epilogue !== undefined) {
      throw new ProtocolDesyncError(
        `Result frame carries runId ${runId ?? '(truncated payload)'} but no run `
        + 'with that id is awaiting a Result on this connection — the runtime is '
        + 'answering a run this client does not know',
      )
    }
    entry.detachAbort()
    entry.detachAbort = NOOP

    if (!peekRunCompletionBackgroundPending(payload)) {
      // The run is over: release any body source still registered
      // (the runtime cancelled its side already).
      this.runs.delete(runId)
      entry.streams?.releaseAll()
      entry.resolve({ result: payload })
      return
    }

    // waitUntil: the value is delivered now (the caller's slot frees); the
    // run keeps going runtime-side. The entry stays in the routing table —
    // grace-time bridge and stream frames keep finding it by run id, beside
    // whatever other runs share the connection — until its RunComplete.
    let settleEpilogue!: (report: DecodedRunComplete | undefined) => void
    const epilogue = new Promise<DecodedRunComplete | undefined>((resolve) => {
      settleEpilogue = resolve
    })
    entry.epilogue = settleEpilogue

    // Aborting during the epilogue cancels the background work gracefully:
    // a Terminate frame truncates the grace phase runtime-side and the
    // RunComplete still arrives (status `truncated`). No teardown fallback —
    // the caller already has their value, and the grace wall bounds a
    // runtime that cannot read the frame.
    const signal = entry.signal
    const cancelGrace = (): void => {
      this.write(
        encodeTsToRustFrame(
          TsToRustMessageTypes.Terminate,
          encodeTerminatePayload(runId),
        ),
      ).catch(() => {
        // Socket already gone — the epilogue resolves through the router.
      })
    }
    if (signal?.aborted) {
      cancelGrace()
    } else if (signal !== undefined) {
      signal.addEventListener('abort', cancelGrace, { once: true })
      entry.detachAbort = () => signal.removeEventListener('abort', cancelGrace)
    }

    entry.resolve({ result: payload, epilogue })
  }

  /**
   * Route a `RunComplete` frame — the end of a run whose Result reported
   * pending `waitUntil` work. One for a run id without a pending epilogue is
   * the same desync signal as a stray `Result`.
   * @param payload
   */
  private routeRunComplete(payload: Uint8Array): void {
    const report = decodeRunCompletePayload(payload)
    const entry = this.runs.get(report.runId)
    if (entry === undefined || entry.epilogue === undefined) {
      throw new ProtocolDesyncError(
        `RunComplete frame carries runId ${report.runId} but no run with that id `
        + 'is in its waitUntil grace phase on this connection',
      )
    }
    this.runs.delete(report.runId)
    entry.detachAbort()
    entry.streams?.releaseAll()
    entry.epilogue(report)
  }

  /**
   * Resolve which run's stream registry a stream frame addresses. Stream ids
   * are per-run, so the run id is the only safe routing key: with several
   * runs in flight a match-by-stream-id fallback could credit or cancel the
   * wrong run's source. The production runtime tags every stream frame with
   * a real run id — run id 0 (the wire's "validation disabled" marker, used
   * only by the direct-fd mode) arriving here means the two sides disagree
   * about what this connection is doing, the same desync contract as a
   * stray Result.
   * @param runId the frame's run id
   */
  private streamOwner(
    runId: number,
  ): { streams: StreamSourceRegistry, runId: number } | undefined {
    if (runId === 0) {
      throw new ProtocolDesyncError(
        'stream frame carries runId 0 — the runtime never leaves stream '
        + 'frames unattributed on a session connection',
      )
    }
    const streams = this.runs.get(runId)?.streams
    return streams === undefined ? undefined : { streams, runId }
  }

  /**
   * Start pumps for every stream registered since the last activation,
   * seeding each with the initial credit window. Called right after the
   * frame carrying the stream handles has been written.
   * @param streams the run's registry
   * @param runId the run the streams belong to
   */
  private activateStreams(streams: StreamSourceRegistry | undefined, runId: number): void {
    if (streams === undefined)
      return
    for (const id of streams.takeNewIds()) {
      const source = streams.sources.get(id)
      if (source === undefined)
        continue
      source.credit = STREAM_CREDIT_WINDOW_BYTES
      this.pumpStream(streams, runId, id)
    }
  }

  /**
   * Pump one stream: send buffered probe chunks first, then pull the source,
   * staying inside the credit window and the per-chunk cap. Fire-and-forget;
   * re-entered by credit grants. Errors end the stream with a `StreamEnd`
   * carrying the message — the sandbox's pending read rejects catchably.
   * @param streams the run's registry
   * @param runId the run
   * @param streamId the stream to pump
   */
  private pumpStream(streams: StreamSourceRegistry, runId: number, streamId: number): void {
    const source = streams.sources.get(streamId)
    if (source === undefined || source.pumping || source.done)
      return
    source.pumping = true;
    (async () => {
      try {
        while (source.credit > 0 && !source.done) {
          let chunk = source.prefix.shift()
          if (chunk === undefined) {
            const { done, value } = await source.reader.read()
            if (done) {
              source.done = true
              streams.sources.delete(streamId)
              await this.write(
                encodeTsToRustFrame(
                  TsToRustMessageTypes.StreamEnd,
                  encodeStreamEndPayload(runId, streamId),
                ),
              )
              return
            }
            chunk = value
          }
          // Respect both the per-chunk cap and the remaining credit; the
          // remainder goes back to the front of the prefix queue.
          const limit = Math.min(STREAM_CHUNK_MAX_BYTES, source.credit)
          if (chunk.byteLength > limit) {
            source.prefix.unshift(chunk.subarray(limit))
            chunk = chunk.subarray(0, limit)
          }
          source.credit -= chunk.byteLength
          await this.write(
            encodeTsToRustFrame(
              TsToRustMessageTypes.StreamChunk,
              encodeStreamChunkPayload(runId, streamId, chunk),
            ),
          )
        }
      } catch (error) {
        source.done = true
        source.reader.cancel().catch(() => {})
        streams.sources.delete(streamId)
        await this.write(
          encodeTsToRustFrame(
            TsToRustMessageTypes.StreamEnd,
            encodeStreamEndPayload(
              runId,
              streamId,
              error instanceof Error ? error.message : String(error),
            ),
          ),
        ).catch(() => {})
      } finally {
        source.pumping = false
      }
    })()
  }

  /**
   * Handle one `BridgeCall` frame: decode, dispatch to the owning run's
   * handler (fire-and-forget), and answer decode/dispatch failures inline.
   *
   * Dispatches are fire-and-forget — the router does NOT await the handler,
   * so it continues routing frames immediately. When the Rust side fires a
   * wall timeout it sends a `Result` frame; the router delivers it without
   * waiting for the handler. The handler promise is then **orphaned**: any
   * in-flight I/O or timers continue to completion, and Rust silently ignores
   * any late `BridgeResponse` that arrives after the run has completed (see
   * session.rs).
   * @param payload the frame payload
   * @param entry the owning run's routing entry
   * @param runId the run the frame belongs to (already peeked by the router)
   */
  private async dispatchBridgeCallFrame(
    payload: Uint8Array,
    entry: RunEntry,
    runId: number,
  ): Promise<void> {
    const { dispatcher, streams } = entry
    // Guest-controlled bytes. A host type the sandbox accepted but this
    // Node refuses to reconstruct (a URL carrying credentials, say)
    // throws here, and the peer is parked waiting for our response — so
    // answer the call with the error instead of letting it escape and
    // cost the connection. `callId` is read before the value blob, so it
    // survives a failure in the blob itself; only a payload too damaged
    // to yield one falls through to the router's teardown.
    let call: ReturnType<typeof decodeBridgeCallPayload>
    try {
      call = decodeBridgeCallPayload(payload)
    } catch (error) {
      const callId = peekBridgeCallId(payload)
      if (callId === undefined)
        throw error
      await this.write(
        encodeTsToRustFrame(
          TsToRustMessageTypes.BridgeResponse,
          encodeBridgeResponsePayload(
            runId,
            callId,
            false,
            undefined,
            {
              name: 'TypeError',
              message: `host could not decode bridge call arguments: ${
                error instanceof Error ? error.message : String(error)
              }`,
            },
          ),
        ),
      )
      return
    }

    if (dispatcher === undefined) {
      // No handler — send a synchronous error response.
      await this.write(
        encodeTsToRustFrame(
          TsToRustMessageTypes.BridgeResponse,
          encodeBridgeResponsePayload(
            call.runId,
            call.callId,
            false,
            undefined,
            { name: 'Error', message: 'no bridge dispatcher configured' },
          ),
        ),
      )
    } else {
      // Fire-and-forget the handler.
      // Do NOT await it — the router reads the next frame immediately.
      // When the handler settles the response is written back.
      // If the run timed out by then, Rust ignores the late frame.
      // Responses echo the frame's own run id — the demux routes by it.
      const { runId: frameRunId, callId } = call
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
            encoded = await serializeHostValue(value, this.brandKey, streams)
          } catch (e) {
            return this.write(
              encodeTsToRustFrame(
                TsToRustMessageTypes.BridgeResponse,
                encodeBridgeResponsePayload(
                  frameRunId,
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
          await this.write(
            encodeTsToRustFrame(
              TsToRustMessageTypes.BridgeResponse,
              encodeBridgeResponsePayload(frameRunId, callId, true, encoded),
            ),
          )
          // The response carrying stream handles is on the wire; start their
          // pumps.
          this.activateStreams(streams, runId)
        },
        (err: unknown) => this.write(
          encodeTsToRustFrame(
            TsToRustMessageTypes.BridgeResponse,
            encodeBridgeResponsePayload(
              frameRunId,
              callId,
              false,
              undefined,
              bridgeErrorPayloadFromUnknown(err),
            ),
          ),
        ),
      ).catch(async (err: unknown) => {
        // Encoding or writing the response itself failed — most reachably
        // `encodeFrame`'s 64 MiB ceiling on a large handler return value.
        // Nothing was written, so the sandbox's awaited bridge promise
        // never settles and the run hangs until the wall deadline, or
        // forever under the supported `wallTimeMs: 0` (Rust installs
        // neither a read timeout nor a wall-guard thread in that case, and
        // freezes the CPU accumulator while parked). Answer with the
        // failure instead: a small payload where the original was not.
        //
        // Safe to send late. This handler is orphaned when the run
        // completes without it, and Rust discards responses for callIds it
        // no longer knows.
        try {
          await this.write(
            encodeTsToRustFrame(
              TsToRustMessageTypes.BridgeResponse,
              encodeBridgeResponsePayload(
                frameRunId,
                callId,
                false,
                undefined,
                bridgeErrorPayloadFromUnknown(err),
              ),
            ),
          )
        } catch {
          // The connection cannot even carry the failure. Tear it down so
          // the run fails fast instead of waiting out the wall clock — but
          // only while the run is still routing here: by now it may have
          // completed and the connection be serving someone else, and
          // killing a healthy connection under a different caller would
          // turn this into the very cross-run fault this branch closes.
          if (this.runs.has(frameRunId))
            this.abortConnection()
        }
      })
    }
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
 * Returns `undefined` when both maps are empty so the router short-circuits to
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
