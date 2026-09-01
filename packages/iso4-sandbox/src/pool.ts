/**
 * Run admission and connection reuse for the sandbox — two separate concerns:
 *
 * - {@link SlotPool} caps how many runs *execute* at once
 *   (`maxConcurrentRuns`); callers beyond it queue FIFO. A slot is a pure
 *   admission ticket — it says nothing about sockets.
 * - {@link ConnectionRegistry} owns the connections: idle ones are reused,
 *   a missing one is opened on demand (nothing connects eagerly), a broken
 *   one is dropped and its replacement opened by whoever needs it next.
 *   Idle connections are kept for the process lifetime; `dispose()` closes
 *   them.
 *
 * {@link RunPool} composes the two: one slot plus one connection per run,
 * both held until the run — including any `waitUntil` grace work — is over.
 * Decoupling capacity from sockets is the point: the admission number is the
 * back-pressure knob, while the connection count simply follows demand.
 * Finer-grained slot release (a slot freed at Result while grace work rides
 * the connection) is the multiplexing activation's job, not this pool's.
 */

import { RunAbortedError } from './client'
import type { RuntimeIpcClient } from './client'

interface SlotWaiter {
  resolve: () => void
  reject: (error: Error) => void
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
  private active = 0
  private readonly waiters: SlotWaiter[] = []
  private disposed = false

  constructor(capacity: number) {
    this.capacity = capacity
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
 * The sandbox's open connections: reused while usable, opened lazily when a
 * caller needs one and nothing idle is available, dropped when broken. There
 * is no connection cap — how many exist is bounded by how many are ever
 * checked out at once, which the {@link SlotPool} already limits.
 */
export class ConnectionRegistry {
  private readonly idle: RuntimeIpcClient[] = []
  /**
   * Connections currently checked out by a caller.
   */
  private leased = 0
  private readonly connect: ConnectFn
  private disposed = false

  constructor(connect: ConnectFn) {
    this.connect = connect
  }

  /**
   * Connections as tracked: idle plus checked out — `stats()` reports this
   * as `openConnections`. A connection that died while idle stays counted
   * until the next `acquire()` observes and drops it, and one still mid-
   * handshake is already counted — the number is the registry's ledger,
   * not a per-read socket probe.
   */
  get openConnections(): number {
    return this.idle.length + this.leased
  }

  /**
   * Reuse an idle connection or open a fresh one. A connection that died
   * while idle (child crash, peer close) is dropped here, not handed out —
   * the drop costs nothing because capacity lives in the slot pool, so the
   * next caller simply opens a replacement. A failed connect fails this
   * caller's run; the registry never shrinks silently.
   */
  async acquire(): Promise<RuntimeIpcClient> {
    if (this.disposed)
      throw new Error('runtime is disposed')

    for (let client = this.idle.pop(); client !== undefined; client = this.idle.pop()) {
      if (client.usable) {
        this.leased++
        return client
      }
      client.dispose().catch(() => {})
    }

    this.leased++
    try {
      return await this.connect()
    } catch (error) {
      this.leased--
      throw error
    }
  }

  /**
   * Return a connection. A usable one goes back to the idle list and is kept
   * for the process lifetime; a broken one is disposed — its replacement is
   * opened on demand by the next caller that needs it.
   * @param client
   */
  release(client: RuntimeIpcClient): void {
    this.leased--
    if (this.disposed || !client.usable) {
      client.dispose().catch(() => {})
      return
    }
    this.idle.push(client)
  }

  async dispose(): Promise<void> {
    if (this.disposed)
      return
    this.disposed = true
    // Checked-out connections are disposed when they come back via release().
    await Promise.all(this.idle.splice(0).map((client) => client.dispose()))
  }
}

/**
 * What `createSandbox` hands to `SandboxImpl`: slot admission composed with
 * connection reuse, exposed through the same borrow-run-return shape the
 * old connection-capacity pool had.
 */
export class RunPool {
  private readonly slots: SlotPool
  private readonly connections: ConnectionRegistry

  constructor(maxConcurrentRuns: number, connect: ConnectFn) {
    this.slots = new SlotPool(maxConcurrentRuns)
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
   * borrow a connection, run `fn`, then return both. The caller's promise
   * settles when `fn` does; a run that ended with pending `waitUntil` work
   * keeps its slot and its connection in the background until the grace
   * phase settles — grace-time frames belong to the finished run, not the
   * next one.
   * @param fn ran with the borrowed connection
   * @param signal the caller's abort signal, honoured while queued
   */
  async withClient<T>(
    fn: (client: RuntimeIpcClient) => Promise<T>,
    signal?: AbortSignal,
  ): Promise<T> {
    await this.slots.acquire(signal)

    let client: RuntimeIpcClient
    try {
      client = await this.connections.acquire()
    } catch (error) {
      this.slots.release()
      throw error
    }

    try {
      return await fn(client)
    } finally {
      (async () => {
        // Re-read until clear: the hold snapshot only covers epilogues
        // pending at read time.
        for (let hold = client.pendingEpilogue; hold !== null; hold = client.pendingEpilogue)
          await hold
        this.connections.release(client)
        this.slots.release()
      })()
    }
  }

  async dispose(): Promise<void> {
    this.slots.dispose()
    await this.connections.dispose()
  }
}
