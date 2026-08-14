/**
 * Connection pool for RuntimeIpcClient slots.
 *
 * Maintains a fixed set of connections (one per maxIsolates slot). Callers
 * use `withClient` to borrow a slot; if all slots are busy the call queues
 * until one is released. The pool is the unit of back-pressure: callers
 * beyond maxIsolates wait here rather than hammering the Rust process.
 */

import type { RuntimeIpcClient } from './client'

interface Waiter {
  resolve: (client: RuntimeIpcClient) => void
  reject: (error: Error) => void
}

/**
 * Opens a fresh connection to replace one that was torn down (e.g. by an
 * in-flight abort). Supplied by `createSandbox`, which holds the socket path
 * and auth token.
 */
export type ConnectFn = () => Promise<RuntimeIpcClient>

export class ConnectionPool {
  private readonly free: RuntimeIpcClient[]
  private readonly waiters: Waiter[] = []
  private readonly connect: ConnectFn | undefined
  private disposed = false

  constructor(clients: RuntimeIpcClient[], connect?: ConnectFn) {
    this.free = [...clients]
    this.connect = connect
  }

  /**
   * Callers currently queued for a free slot — `stats()` reports this as
   * `queueDepth`.
   */
  get queueDepth(): number {
    return this.waiters.length
  }

  /**
   * Borrow a client, run `fn`, then unconditionally return the client.
   * If the pool is disposed while `fn` is running, the client is disposed
   * on release rather than returned to the free list.
   * @param fn
   */
  async withClient<T>(fn: (client: RuntimeIpcClient) => Promise<T>): Promise<T> {
    const client = await this.acquire()
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

    // Reject any callers waiting for a free slot.
    for (const { reject } of this.waiters.splice(0)) {
      reject(new Error('runtime disposed'))
    }

    // Dispose all currently free connections. In-flight connections are
    // disposed when they are returned via release().
    await Promise.all(this.free.splice(0).map((c) => c.dispose()))
  }

  private acquire(): Promise<RuntimeIpcClient> {
    if (this.disposed) {
      return Promise.reject(new Error('runtime is disposed'))
    }

    const client = this.free.pop()
    if (client !== undefined) {
      return Promise.resolve(client)
    }

    // All slots busy — queue until one is released. A bounded wait in
    // practice: every run has wall/CPU limits, so slots always free up.
    return new Promise((resolve, reject) => {
      this.waiters.push({ resolve, reject })
    })
  }

  private release(client: RuntimeIpcClient): void {
    if (this.disposed) {
      // Pool was disposed while this slot was in use — dispose it now.
      client.dispose()
      return
    }

    // A client torn down by an in-flight abort must not be reused. Drop it and
    // open a replacement so the pool keeps its full complement of slots.
    if (!client.usable) {
      // `replace` never rejects (it swallows its own errors); the trailing
      // catch is a belt-and-suspenders guard against an unhandled rejection.
      this.replace(client).catch(() => {})
      return
    }

    const next = this.waiters.shift()
    if (next !== undefined) {
      next.resolve(client)
    } else {
      this.free.push(client)
    }
  }

  /**
   * Dispose a dead slot and connect a fresh one in its place. If reconnection
   * fails (or no factory was supplied) the pool simply runs with one fewer
   * slot rather than handing back a broken connection; any waiter still gets
   * served by other slots as they free up.
   * @param dead
   */
  private async replace(dead: RuntimeIpcClient): Promise<void> {
    await dead.dispose().catch(() => {})
    if (this.connect === undefined)
      return
    let fresh: RuntimeIpcClient
    try {
      fresh = await this.connect()
    } catch {
      // Could not reopen the slot — leave the pool one short. Subsequent runs
      // still succeed on the remaining slots.
      return
    }
    if (this.disposed) {
      // Pool was disposed while reconnecting — don't leak the new connection.
      await fresh.dispose().catch(() => {})
      return
    }
    this.release(fresh)
  }
}
