/**
 * ConnectionPool: capacity is a number, not a fixed set of connections.
 *
 * A dead connection is dropped and its capacity reopened on demand, so a
 * transient connect failure fails one run instead of permanently costing the
 * pool a slot. Removing the fixed slot set entirely is #89.
 */

import { describe, expect, test, vi } from 'vitest'

import { ConnectionPool } from '../src/pool'
import { RunAbortedError } from '../src/client'
import type { RuntimeIpcClient } from '../src/client'

/**
 * Minimal stand-in for a pooled connection: `usable` is the only thing the pool
 * reads, and `dispose` the only thing it calls.
 * @param usable whether the fake reports itself as reusable
 */
function fakeClient(usable = true): RuntimeIpcClient {
  return {
    usable,
    dispose: vi.fn(async () => {}),
  } as unknown as RuntimeIpcClient
}

/**
 * Hold the pool's only connection until `release()` is called. `started`
 * resolves once the callback is genuinely running and the connection is
 * checked out — `withClient` awaits `acquire`, so that is a microtask later
 * than this call, and releasing before it would hit the placeholder.
 * @param pool the pool whose connection to hold
 * @param onRelease ran inside the callback, after the release and before it returns
 */
function hold(pool: ConnectionPool, onRelease?: () => void): {
  started: Promise<void>
  release: () => void
  done: Promise<void>
} {
  let release: () => void = () => {}
  let markStarted: () => void = () => {}
  const started = new Promise<void>((r) => {
    markStarted = r
  })
  const done = pool.withClient(async () => {
    markStarted()
    await new Promise<void>((r) => {
      release = r
    })
    onRelease?.()
  })
  return { started, release: () => release(), done }
}

describe('ConnectionPool capacity', () => {
  test('drops a dead connection and opens a fresh one for the next caller', async () => {
    const dead = fakeClient(false)
    const fresh = fakeClient()
    const connect = vi.fn().mockResolvedValue(fresh)
    const pool = new ConnectionPool([dead], connect)

    // Borrow and return the dead client. No reconnect happens here.
    await expect(pool.withClient(async (c) => c)).resolves.toBe(dead)
    expect(dead.dispose).toHaveBeenCalled()
    expect(connect).not.toHaveBeenCalled()

    // The next caller needs a connection, so one is opened now.
    await expect(pool.withClient(async (c) => c)).resolves.toBe(fresh)
    expect(connect).toHaveBeenCalledTimes(1)
  })

  test('a failed connect fails that run and leaves capacity for the next', async () => {
    const fresh = fakeClient()
    const connect = vi.fn()
      .mockRejectedValueOnce(new Error('ECONNREFUSED'))
      .mockResolvedValueOnce(fresh)
    const pool = new ConnectionPool([fakeClient(false)], connect)

    await pool.withClient(async (c) => c) // retires the dead client

    await expect(pool.withClient(async (c) => c)).rejects.toThrow(/ECONNREFUSED/)
    // Capacity was given back, so the next call is not stuck behind the failure.
    await expect(pool.withClient(async (c) => c)).resolves.toBe(fresh)
  })

  test('never exceeds capacity, even when acquires race', async () => {
    const connect = vi.fn(async () => fakeClient())
    // Capacity 2, both connections dead so the pool must reopen both.
    const pool = new ConnectionPool([fakeClient(false), fakeClient(false)], connect)
    await Promise.all([
      pool.withClient(async (c) => c),
      pool.withClient(async (c) => c),
    ])

    let peak = 0
    let open = 0
    await Promise.all(Array.from({ length: 8 }, () => pool.withClient(async () => {
      open++
      peak = Math.max(peak, open)
      await new Promise((r) => {
        setTimeout(r, 5)
      })
      open--
    })))

    expect(peak).toBeLessThanOrEqual(2)
  })

  test('a queued caller is served by capacity freed from a dead connection', async () => {
    const fresh = fakeClient()
    const connect = vi.fn().mockResolvedValue(fresh)
    // Capacity 1, and the one connection dies while in use — the way an
    // in-flight abort or a peer close leaves it.
    const dying = fakeClient(true)
    const pool = new ConnectionPool([dying], connect)
    const held = hold(pool, () => {
      ;(dying as { usable: boolean }).usable = false
    })
    await held.started

    const queued = pool.withClient(async (c) => c)
    expect(pool.queueDepth).toBe(1)

    held.release()
    await held.done
    await expect(queued).resolves.toBe(fresh)
  })

  test('a queued caller sees the connect failure instead of waiting forever', async () => {
    const connect = vi.fn().mockRejectedValue(new Error('child is gone'))
    const dying = fakeClient(true)
    const pool = new ConnectionPool([dying], connect)
    const held = hold(pool, () => {
      ;(dying as { usable: boolean }).usable = false
    })
    await held.started

    const queued = pool.withClient(async (c) => c)

    held.release()
    await held.done
    await expect(queued).rejects.toThrow(/child is gone/)
  })
})

describe('ConnectionPool queued callers', () => {
  test('a queued caller is released by its AbortSignal', async () => {
    // Without the signal wired into `acquire` this promise had no timeout and
    // no rejection path, so the caller waited forever.
    const pool = new ConnectionPool([fakeClient()])
    const controller = new AbortController()
    const held = hold(pool)
    await held.started

    const queued = pool.withClient(async (c) => c, controller.signal)
    controller.abort(new Error('caller went away'))

    await expect(queued).rejects.toBeInstanceOf(RunAbortedError)

    held.release()
    await held.done
  })

  test('an aborted waiter leaves the queue, so the connection goes to the next caller', async () => {
    const only = fakeClient()
    const pool = new ConnectionPool([only])
    const controller = new AbortController()
    const held = hold(pool)
    await held.started

    const abandoned = pool.withClient(async (c) => c, controller.signal)
    const waiting = pool.withClient(async (c) => c)

    controller.abort()
    await expect(abandoned).rejects.toBeInstanceOf(RunAbortedError)

    held.release()
    await held.done
    await expect(waiting).resolves.toBe(only)
  })

  test('queueDepth counts only callers still waiting', async () => {
    const pool = new ConnectionPool([fakeClient()])
    const controller = new AbortController()
    const held = hold(pool)
    await held.started

    const queued = pool.withClient(async (c) => c, controller.signal)
    expect(pool.queueDepth).toBe(1)

    controller.abort()
    await expect(queued).rejects.toBeInstanceOf(RunAbortedError)
    expect(pool.queueDepth).toBe(0)

    held.release()
    await held.done
  })
})
