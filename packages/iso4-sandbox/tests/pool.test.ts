/**
 * Run admission and connection reuse, as two separate concerns:
 *
 * - `SlotPool` caps runs executing at once and queues the rest FIFO —
 *   capacity is an admission number, never a set of objects.
 * - `ConnectionRegistry` opens connections lazily on demand, reuses idle
 *   ones, and drops broken ones so the next caller opens a replacement —
 *   a transient connect failure fails one run instead of permanently
 *   costing capacity.
 * - `RunPool` composes them: one slot plus one connection per run, both
 *   held until the run (waitUntil grace work included) is over.
 */

import { describe, expect, test, vi } from 'vitest'

import { ConnectionRegistry, RunPool, SlotPool } from '../src/pool'
import { RunAbortedError } from '../src/client'
import type { RuntimeIpcClient } from '../src/client'

/**
 * Minimal stand-in for a pooled connection: `usable` and `pendingEpilogue`
 * are the only things the pool reads, and `dispose` the only thing it calls.
 * @param usable whether the fake reports itself as reusable
 */
function fakeClient(usable = true): RuntimeIpcClient {
  return {
    usable,
    pendingEpilogue: null,
    dispose: vi.fn(async () => {}),
  } as unknown as RuntimeIpcClient
}

/**
 * Hold one of the pool's run slots until `release()` is called. `started`
 * resolves once the callback is genuinely running — `withClient` awaits the
 * slot and the connection, so that is microtasks later than this call.
 * @param pool the pool to hold a slot of
 * @param onRelease ran inside the callback, after the release and before it returns
 */
function hold(pool: RunPool, onRelease?: () => void): {
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

describe('SlotPool admission', () => {
  test('admits up to capacity and queues the rest FIFO', async () => {
    const slots = new SlotPool(2)
    await slots.acquire()
    await slots.acquire()
    expect(slots.queueDepth).toBe(0)

    const order: number[] = []
    const third = slots.acquire().then(() => order.push(3))
    const fourth = slots.acquire().then(() => order.push(4))
    expect(slots.queueDepth).toBe(2)

    slots.release()
    await third
    slots.release()
    await fourth
    // FIFO: the longest-queued caller got the first freed slot.
    expect(order).toEqual([3, 4])
    expect(slots.queueDepth).toBe(0)
  })

  test('an already-aborted signal is rejected before admission or queueing', async () => {
    // `addEventListener` never fires for a signal that already aborted, so
    // without the entry check a pre-aborted caller (its AbortSignal.timeout
    // expired during argument serialization, say) would sit queued until an
    // unrelated run freed a slot.
    const slots = new SlotPool(1)
    const controller = new AbortController()
    controller.abort(new Error('expired before admission'))

    // Rejected even though capacity is free — the run must not be admitted.
    await expect(slots.acquire(controller.signal)).rejects.toBeInstanceOf(RunAbortedError)

    // The free slot was not consumed by the rejected caller.
    await expect(slots.acquire()).resolves.toBeUndefined()
  })

  test('a queued caller is released by its AbortSignal', async () => {
    const slots = new SlotPool(1)
    await slots.acquire()
    const controller = new AbortController()

    const queued = slots.acquire(controller.signal)
    controller.abort(new Error('caller went away'))

    await expect(queued).rejects.toBeInstanceOf(RunAbortedError)
  })

  test('an aborted waiter leaves the queue, so the slot goes to the next caller', async () => {
    const slots = new SlotPool(1)
    await slots.acquire()
    const controller = new AbortController()

    const abandoned = slots.acquire(controller.signal)
    const waiting = slots.acquire()

    controller.abort()
    await expect(abandoned).rejects.toBeInstanceOf(RunAbortedError)

    slots.release()
    await expect(waiting).resolves.toBeUndefined()
  })

  test('queueDepth counts only callers still waiting', async () => {
    const slots = new SlotPool(1)
    await slots.acquire()
    const controller = new AbortController()

    const queued = slots.acquire(controller.signal)
    expect(slots.queueDepth).toBe(1)

    controller.abort()
    await expect(queued).rejects.toBeInstanceOf(RunAbortedError)
    expect(slots.queueDepth).toBe(0)
  })

  test('dispose rejects queued callers and refuses new ones', async () => {
    const slots = new SlotPool(1)
    await slots.acquire()
    const queued = slots.acquire()

    slots.dispose()
    await expect(queued).rejects.toThrow(/runtime disposed/)
    await expect(slots.acquire()).rejects.toThrow(/runtime is disposed/)
  })
})

describe('ConnectionRegistry', () => {
  test('opens lazily, reuses the returned connection, and counts both states', async () => {
    const fresh = fakeClient()
    const connect = vi.fn().mockResolvedValue(fresh)
    const registry = new ConnectionRegistry(connect)
    expect(registry.openConnections).toBe(0)

    const first = await registry.acquire()
    expect(first).toBe(fresh)
    expect(connect).toHaveBeenCalledTimes(1)
    expect(registry.openConnections).toBe(1)

    registry.release(first)
    expect(registry.openConnections).toBe(1)

    // Reused, not reopened.
    await expect(registry.acquire()).resolves.toBe(fresh)
    expect(connect).toHaveBeenCalledTimes(1)
  })

  test('drops a connection that died while idle and opens a fresh one', async () => {
    const dying = fakeClient(true)
    const fresh = fakeClient()
    const connect = vi.fn()
      .mockResolvedValueOnce(dying)
      .mockResolvedValueOnce(fresh)
    const registry = new ConnectionRegistry(connect)

    registry.release(await registry.acquire())
    ;(dying as { usable: boolean }).usable = false

    await expect(registry.acquire()).resolves.toBe(fresh)
    expect(dying.dispose).toHaveBeenCalled()
    expect(registry.openConnections).toBe(1)
  })

  test('a connection returned broken is disposed, not pooled', async () => {
    const broken = fakeClient(false)
    const registry = new ConnectionRegistry(vi.fn().mockResolvedValue(broken))

    const client = await registry.acquire()
    registry.release(client)

    expect(broken.dispose).toHaveBeenCalled()
    expect(registry.openConnections).toBe(0)
  })

  test('a failed connect fails that caller and leaves the count intact', async () => {
    const fresh = fakeClient()
    const connect = vi.fn()
      .mockRejectedValueOnce(new Error('ECONNREFUSED'))
      .mockResolvedValueOnce(fresh)
    const registry = new ConnectionRegistry(connect)

    await expect(registry.acquire()).rejects.toThrow(/ECONNREFUSED/)
    expect(registry.openConnections).toBe(0)
    await expect(registry.acquire()).resolves.toBe(fresh)
  })

  test('dispose closes idle connections and disposes later returns', async () => {
    const idle = fakeClient()
    const leased = fakeClient()
    const connect = vi.fn()
      .mockResolvedValueOnce(idle)
      .mockResolvedValueOnce(leased)
    const registry = new ConnectionRegistry(connect)

    registry.release(await registry.acquire())
    const inFlight = await registry.acquire() // takes `idle` back out
    const second = await registry.acquire() // opens `leased`
    registry.release(inFlight)

    await registry.dispose()
    expect(inFlight.dispose).toHaveBeenCalled()

    registry.release(second)
    expect(second.dispose).toHaveBeenCalled()
    await expect(registry.acquire()).rejects.toThrow(/runtime is disposed/)
  })
})

describe('RunPool composition', () => {
  test('never runs more than the slot capacity, even when acquires race', async () => {
    const pool = new RunPool(2, async () => fakeClient())

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

  test('slots admit runs while connections follow demand', async () => {
    const clients = [fakeClient(), fakeClient()]
    const connect = vi.fn(async () => clients[connect.mock.calls.length - 1] ?? fakeClient())
    const pool = new RunPool(2, connect)
    expect(pool.openConnections).toBe(0)

    // Two concurrent runs open two connections; both are kept afterwards.
    const first = hold(pool)
    const second = hold(pool)
    await first.started
    await second.started
    expect(pool.openConnections).toBe(2)

    first.release()
    second.release()
    await first.done
    await second.done
    expect(pool.openConnections).toBe(2)

    // A later run reuses an idle connection instead of opening a third.
    await pool.withClient(async () => {})
    expect(connect).toHaveBeenCalledTimes(2)
  })

  test('a connect failure fails that run and frees its slot for the next', async () => {
    const fresh = fakeClient()
    const connect = vi.fn()
      .mockRejectedValueOnce(new Error('child is gone'))
      .mockResolvedValueOnce(fresh)
    const pool = new RunPool(1, connect)

    await expect(pool.withClient(async (c) => c)).rejects.toThrow(/child is gone/)
    // The slot was given back, so the next call is not stuck behind the failure.
    await expect(pool.withClient(async (c) => c)).resolves.toBe(fresh)
  })

  test('a queued caller gets a fresh connection when the previous one broke mid-run', async () => {
    const dying = fakeClient(true)
    const fresh = fakeClient()
    const connect = vi.fn()
      .mockResolvedValueOnce(dying)
      .mockResolvedValueOnce(fresh)
    const pool = new RunPool(1, connect)

    const held = hold(pool, () => {
      ;(dying as { usable: boolean }).usable = false
    })
    await held.started

    const queued = pool.withClient(async (c) => c)
    expect(pool.queueDepth).toBe(1)

    held.release()
    await held.done
    await expect(queued).resolves.toBe(fresh)
    expect(dying.dispose).toHaveBeenCalled()
  })

  test('a run with pending waitUntil work holds its slot and connection until the grace phase ends', async () => {
    const client = fakeClient()
    let settleEpilogue: () => void = () => {}
    let epilogueHold: Promise<void> | null = new Promise<void>((r) => {
      settleEpilogue = () => {
        epilogueHold = null
        r()
      }
    })
    Object.defineProperty(client, 'pendingEpilogue', {
      get: () => epilogueHold,
    })
    const pool = new RunPool(1, vi.fn().mockResolvedValue(client))

    // The run itself returns immediately — the value was delivered — but the
    // slot must not admit the queued caller until the epilogue settles.
    await pool.withClient(async () => {})
    const queued = pool.withClient(async (c) => c)

    await new Promise((r) => {
      setTimeout(r, 10)
    })
    expect(pool.queueDepth).toBe(1)

    settleEpilogue()
    await expect(queued).resolves.toBe(client)
  })

  test('dispose rejects queued callers', async () => {
    const pool = new RunPool(1, async () => fakeClient())
    const held = hold(pool)
    await held.started
    const queued = pool.withClient(async (c) => c)

    const disposed = pool.dispose()
    await expect(queued).rejects.toThrow(/runtime disposed/)

    held.release()
    await held.done
    await disposed
  })
})
