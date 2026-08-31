/**
 * Connection pool for RuntimeIpcClient slots.
 *
 * `maxIsolates` is a *capacity*, not a fixed set of objects: the pool holds at
 * most that many live connections, reuses idle ones, and opens a fresh one when
 * capacity is free and nothing idle is available. Callers beyond capacity queue
 * here rather than hammering the Rust process — the pool is the unit of
 * back-pressure.
 *
 * A connection that comes back dead (in-flight abort, peer close, child crash)
 * is dropped, not replaced on the spot. Capacity is a number, so the drop frees
 * a unit of it and the next caller that needs a connection opens one. That is
 * why a failed connect fails a *run* rather than permanently costing the pool a
 * slot. Removing the fixed slot set entirely is a planned pool rework.
 */

import { RunAbortedError } from './client'
import type { RuntimeIpcClient } from './client'

interface Waiter {
  resolve: (client: RuntimeIpcClient) => void
  reject: (error: Error) => void
}

/**
 * Opens a fresh connection. Supplied by `createSandbox`, which holds the socket
 * path.
 */
export type ConnectFn = () => Promise<RuntimeIpcClient>

export class ConnectionPool {
  private readonly idle: RuntimeIpcClient[]
  private readonly waiters: Waiter[] = []
  private readonly connect: ConnectFn | undefined
  /**
   * Upper bound on live connections — `maxIsolates`.
   */
  private readonly capacity: number
  /**
   * Connections currently checked out by a caller.
   */
  private leased = 0
  private disposed = false

  constructor(clients: RuntimeIpcClient[], connect?: ConnectFn) {
    this.idle = [...clients]
    this.capacity = clients.length
    this.connect = connect
  }

  /**
   * Callers currently queued for a connection — `stats()` reports this as
   * `queueDepth`.
   */
  get queueDepth(): number {
    return this.waiters.length
  }

  /**
   * Borrow a client, run `fn`, then unconditionally return the client.
   * If the pool is disposed while `fn` is running, the client is disposed
   * on release rather than returned to the idle list.
   * @param fn ran with the borrowed connection
   * @param signal
   *   The caller's abort signal, honoured *while queued*. Per-run
   *   `wallTimeMs` / `cpuTimeMs` cannot bound that wait: the caller is upstream
   *   of the runtime and its `Run` frame has not been sent yet.
   */
  async withClient<T>(
    fn: (client: RuntimeIpcClient) => Promise<T>,
    signal?: AbortSignal,
  ): Promise<T> {
    const client = await this.acquire(signal)
    try {
      return await fn(client)
    } finally {
      this.release(client)
    }
  }

  async dispose(): Promise<void> {
    if (this.disposed)
      return
    this.disposed = true

    for (const { reject } of this.waiters.splice(0)) {
      reject(new Error('runtime disposed'))
    }

    // In-flight connections are disposed when they are returned via release().
    await Promise.all(this.idle.splice(0).map((c) => c.dispose()))
  }

  /**
   * Live connections: idle plus checked out.
   */
  private get live(): number {
    return this.idle.length + this.leased
  }

  private acquire(signal?: AbortSignal): Promise<RuntimeIpcClient> {
    if (this.disposed)
      return Promise.reject(new Error('runtime is disposed'))

    const reused = this.idle.pop()
    if (reused !== undefined) {
      this.leased++
      return Promise.resolve(reused)
    }

    if (this.live < this.capacity)
      return this.open()

    return this.queue(signal)
  }

  /**
   * Open a connection against free capacity. Counts the lease before awaiting,
   * so concurrent acquires cannot both read the same free capacity and overshoot
   * `maxIsolates`. A failure hands the error to this caller: the run fails and
   * the next one tries again, rather than the pool quietly shrinking.
   */
  private open(): Promise<RuntimeIpcClient> {
    if (this.connect === undefined) {
      return Promise.reject(
        new Error('[@iso4/sandbox] no runtime connection available'),
      )
    }
    this.leased++
    return this.connect().catch((error: unknown) => {
      this.leased--
      throw error
    })
  }

  private queue(signal?: AbortSignal): Promise<RuntimeIpcClient> {
    return new Promise<RuntimeIpcClient>((resolve, reject) => {
      const waiter: Waiter = { resolve, reject }
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

      waiter.resolve = (client) => {
        settle()
        resolve(client)
      }
      waiter.reject = (error) => {
        settle()
        reject(error)
      }
      this.waiters.push(waiter)
    })
  }

  private release(client: RuntimeIpcClient): void {
    // A run that ended with pending waitUntil work still owns its
    // connection: grace-time bridge frames and the final RunComplete belong
    // to it. Hold the slot until the epilogue settles, then release for real.
    const hold = client.pendingEpilogue
    if (hold) {
      hold.then(() => {
        client.pendingEpilogue = null
        this.releaseNow(client)
      })
      return
    }
    this.releaseNow(client)
  }

  private releaseNow(client: RuntimeIpcClient): void {
    this.leased--

    if (this.disposed) {
      client.dispose().catch(() => {})
      return
    }

    if (!client.usable) {
      // Dead: drop it and let the freed capacity be filled on demand.
      client.dispose().catch(() => {})
      this.fillWaiters()
      return
    }

    const next = this.waiters.shift()
    if (next !== undefined) {
      this.leased++
      next.resolve(client)
    } else {
      this.idle.push(client)
    }
  }

  /**
   * Hand freed capacity to queued callers by opening connections for them. A
   * failure rejects that caller instead of vanishing, so a dead child surfaces
   * as failing runs rather than an unbounded queue.
   */
  private fillWaiters(): void {
    while (this.waiters.length > 0 && this.live < this.capacity) {
      const waiter = this.waiters.shift()
      if (waiter === undefined)
        return
      this.open().then(waiter.resolve, waiter.reject)
    }
  }
}
