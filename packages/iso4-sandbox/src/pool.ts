/**
 * Run admission and connection sharing for the sandbox — two separate
 * concerns:
 *
 * - {@link SlotPool} caps how many runs *execute* at once
 *   (`maxConcurrentRuns`); callers beyond it queue FIFO. A slot is a pure
 *   admission ticket, held from admission until the run's Result — a run
 *   whose Result reported pending `waitUntil` work releases its slot there,
 *   and the grace work rides the connection in the background: admission
 *   caps foreground execution, not epilogues.
 * - {@link ConnectionRegistry} shares connections: frames are multiplexed by
 *   run id, so one connection carries up to {@link RUNS_PER_CONNECTION}
 *   concurrent runs, and a new one opens only when every open connection is
 *   at the cap. The cap bounds the blast radius of a connection-level death
 *   (protocol desync, outbound stall, socket error) to that many runs.
 *   Connections are kept for the process lifetime; `dispose()` closes them.
 *
 * {@link RunPool} composes the two: a slot plus a shared connection per run.
 * Decoupling capacity from sockets is the point — the admission number is
 * the back-pressure knob, while the connection count follows demand at
 * roughly ceil(concurrent runs / cap).
 */

import { RunAbortedError } from './client'
import type { RuntimeIpcClient } from './client'

/**
 * How many concurrent runs one connection carries before another is opened.
 * Internal for now — the #127 bench matrix picks the shipped default; 1
 * reproduces the pre-multiplexing one-run-per-connection topology exactly.
 */
export const RUNS_PER_CONNECTION = 4

interface SlotWaiter {
  resolve: () => void
  reject: (error: Error) => void
}

/**
 * The waiter queue is at `maxQueuedRuns`: the run is shed as a failed
 * result with `error.code: 'ERR_QUEUE_FULL'` instead of growing an
 * unbounded, memory-holding queue.
 */
export class QueueFullError extends Error {
  constructor(message: string) {
    super(message)
    this.name = 'QueueFullError'
  }
}

/**
 * Opens a fresh connection. Supplied by `createSandbox`, which holds the socket
 * path.
 */
export type ConnectFn = () => Promise<RuntimeIpcClient>

/**
 * FIFO admission over a fixed number of run slots. Purely a counter — it
 * never touches connections.
 */
export class SlotPool {
  private readonly capacity: number
  private readonly maxQueued: number
  private active = 0
  private readonly waiters: SlotWaiter[] = []
  private disposed = false

  constructor(capacity: number, maxQueued: number) {
    this.capacity = capacity
    this.maxQueued = maxQueued
  }

  /**
   * Callers currently queued for a slot — `stats()` reports this as
   * `queueDepth`.
   */
  get queueDepth(): number {
    return this.waiters.length
  }

  /**
   * Take a slot, or queue FIFO until one frees.
   * @param signal
   *   The caller's abort signal, honoured *while queued*. Per-run
   *   `wallTimeMs` / `cpuTimeMs` cannot bound that wait: the caller is
   *   upstream of the runtime and its `Run` frame has not been sent yet.
   *   `AbortSignal.timeout()` composes as a queue-wait bound — there is no
   *   separate timeout knob.
   */
  acquire(signal?: AbortSignal): Promise<void> {
    if (this.disposed)
      return Promise.reject(new Error('runtime is disposed'))

    // A signal that fired before admission (during argument serialization,
    // say) must reject here: `addEventListener` never fires for an
    // already-aborted signal, so queueing would strand the caller until an
    // unrelated run frees a slot.
    if (signal?.aborted)
      return Promise.reject(new RunAbortedError(signal.reason))

    if (this.active < this.capacity) {
      this.active++
      return Promise.resolve()
    }

    if (this.waiters.length >= this.maxQueued) {
      return Promise.reject(new QueueFullError(
        `run queue is full (${this.waiters.length} queued behind `
        + `${this.capacity} executing runs, maxQueuedRuns: ${this.maxQueued}); `
        + 'the sandbox is overloaded — retry later, or raise maxQueuedRuns',
      ))
    }

    return new Promise<void>((resolve, reject) => {
      const waiter: SlotWaiter = { resolve, reject }
      if (signal === undefined) {
        this.waiters.push(waiter)
        return
      }

      const onAbort = (): void => {
        const at = this.waiters.indexOf(waiter)
        if (at !== -1)
          this.waiters.splice(at, 1)
        reject(new RunAbortedError(signal.reason))
      }
      signal.addEventListener('abort', onAbort, { once: true })
      const settle = (): void => signal.removeEventListener('abort', onAbort)

      waiter.resolve = () => {
        settle()
        resolve()
      }
      waiter.reject = (error) => {
        settle()
        reject(error)
      }
      this.waiters.push(waiter)
    })
  }

  /**
   * Free a slot. The longest-queued caller takes it directly; only when
   * nobody waits does the active count drop.
   */
  release(): void {
    const next = this.waiters.shift()
    if (next !== undefined) {
      next.resolve()
      return
    }
    this.active--
  }

  dispose(): void {
    if (this.disposed)
      return
    this.disposed = true
    for (const { reject } of this.waiters.splice(0))
      reject(new Error('runtime disposed'))
  }
}

/**
 * The sandbox's open connections, shared by run id multiplexing: each
 * carries up to {@link RUNS_PER_CONNECTION} concurrent runs, one is opened
 * lazily when every open connection is at the cap, and a broken one is
 * dropped when observed so the next caller opens a replacement.
 */
export class ConnectionRegistry {
  /**
   * Every open connection — `stats()` reports this (plus in-flight opens)
   * as `openConnections`. A connection that died stays counted until the
   * next `tryAcquire()` observes and drops it — the number is the
   * registry's ledger, not a per-read socket probe.
   */
  private readonly connections: RuntimeIpcClient[] = []
  /**
   * Connects in flight — counted in the ledger so a burst of opens is
   * visible, not a blind spot.
   */
  private opening = 0
  private readonly connect: ConnectFn
  private disposed = false

  constructor(connect: ConnectFn) {
    this.connect = connect
  }

  get openConnections(): number {
    return this.connections.length + this.opening
  }

  /**
   * A usable connection below the per-connection run cap, or `undefined`
   * when every one is full (the caller opens a fresh one via {@link open}).
   *
   * Synchronous on purpose: the caller starts its run in the same tick, and
   * run registration in the client is synchronous from that call — so the
   * load this observed cannot go stale under it, and the cap is exact
   * rather than best-effort. First-fit keeps packing deterministic:
   * connection count settles at ceil(active runs / cap). A connection that
   * died is dropped here, not handed out.
   */
  tryAcquire(): RuntimeIpcClient | undefined {
    if (this.disposed)
      throw new Error('runtime is disposed')
    for (let i = 0; i < this.connections.length; i++) {
      const client = this.connections[i]!
      if (!client.usable) {
        client.dispose().catch(() => {})
        this.connections.splice(i, 1)
        i--
        continue
      }
      if (client.load < RUNS_PER_CONNECTION)
        return client
    }
    return undefined
  }

  /**
   * Open a fresh connection and add it to the shared set. Concurrent
   * callers that all found the set full each open one — a cold burst can
   * briefly open more connections than the steady-state packing needs;
   * they are kept and reused, so the count follows demand from then on. A
   * failed connect fails this caller's run; the registry never shrinks
   * silently.
   */
  async open(): Promise<RuntimeIpcClient> {
    if (this.disposed)
      throw new Error('runtime is disposed')
    this.opening++
    try {
      const client = await this.connect()
      if (this.disposed) {
        client.dispose().catch(() => {})
        throw new Error('runtime is disposed')
      }
      this.connections.push(client)
      return client
    } finally {
      this.opening--
    }
  }

  async dispose(): Promise<void> {
    if (this.disposed)
      return
    this.disposed = true
    // Abrupt by design: the sandbox kills the child right after, so there
    // is nothing to drain for.
    await Promise.all(this.connections.splice(0).map((client) => client.dispose()))
  }
}

/**
 * What `createSandbox` hands to `SandboxImpl`: slot admission composed with
 * connection sharing, exposed through the same borrow-run shape as before.
 */
export class RunPool {
  private readonly slots: SlotPool
  private readonly connections: ConnectionRegistry

  constructor(maxConcurrentRuns: number, maxQueuedRuns: number, connect: ConnectFn) {
    this.slots = new SlotPool(maxConcurrentRuns, maxQueuedRuns)
    this.connections = new ConnectionRegistry(connect)
  }

  get queueDepth(): number {
    return this.slots.queueDepth
  }

  get openConnections(): number {
    return this.connections.openConnections
  }

  /**
   * Take a run slot (queueing FIFO when `maxConcurrentRuns` are executing),
   * pick a shared connection, run `fn`, then release the slot when `fn`
   * settles — which is at the run's Result. A run that ended with pending
   * `waitUntil` work holds no slot during its grace phase: the grace frames
   * ride the shared connection, routed by run id, until its RunComplete.
   * @param fn ran with the shared connection
   * @param signal the caller's abort signal, honoured while queued
   */
  async withClient<T>(
    fn: (client: RuntimeIpcClient) => Promise<T>,
    signal?: AbortSignal,
  ): Promise<T> {
    await this.slots.acquire(signal)

    let client: RuntimeIpcClient
    try {
      client = this.connections.tryAcquire() ?? await this.connections.open()
    } catch (error) {
      this.slots.release()
      throw error
    }

    try {
      return await fn(client)
    } finally {
      this.slots.release()
    }
  }

  async dispose(): Promise<void> {
    this.slots.dispose()
    await this.connections.dispose()
  }
}
