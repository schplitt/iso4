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

export class ConnectionPool {
  private readonly free: RuntimeIpcClient[]
  private readonly waiters: Waiter[] = []
  private disposed = false

  constructor(clients: RuntimeIpcClient[]) {
    this.free = [...clients]
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

    // All slots busy — queue until one is released.
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

    const next = this.waiters.shift()
    if (next !== undefined) {
      next.resolve(client)
    } else {
      this.free.push(client)
    }
  }
}
