/**
 * Run admission and connection sharing, as two separate concerns:
 *
 * - `SlotPool` caps runs executing at once and queues the rest FIFO —
 *   capacity is an admission number, never a set of objects. The slot frees
 *   at the run's Result; waitUntil grace work holds none.
 * - `ConnectionRegistry` shares connections up to `RUNS_PER_CONNECTION`
 *   concurrent runs each, opens another lazily when all are full, and drops
 *   broken ones so the next caller opens a replacement — a transient connect
 *   failure fails one run instead of permanently costing capacity.
 * - `RunPool` composes them: a slot plus a shared connection per run.
 */

import { describe, expect, test, vi } from 'vitest'

import { ConnectionRegistry, QueueFullError, RUNS_PER_CONNECTION, RunPool, SlotPool } from '../src/pool'
import { RunAbortedError } from '../src/client'
import type { RuntimeIpcClient } from '../src/client'

/**
 * Minimal stand-in for a shared connection: `usable` and `load` are
 * the only things the pool reads, and `dispose` the only thing it calls.
 * @param usable whether the fake reports itself as reusable
 */
function fakeClient(usable = true): RuntimeIpcClient & { usable: boolean, load: number } {
  return {
    usable,
    load: 0,
    dispose: vi.fn(async () => {}),
  } as unknown as RuntimeIpcClient & { usable: boolean, load: number }
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
    const slots = new SlotPool(2, 1000)
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
    const slots = new SlotPool(1, 1000)
    const controller = new AbortController()
    controller.abort(new Error('expired before admission'))

    // Rejected even though capacity is free — the run must not be admitted.
    await expect(slots.acquire(controller.signal)).rejects.toBeInstanceOf(RunAbortedError)

    // The free slot was not consumed by the rejected caller.
    await expect(slots.acquire()).resolves.toBeUndefined()
  })

  test('a queued caller is released by its AbortSignal', async () => {
    const slots = new SlotPool(1, 1000)
    await slots.acquire()
    const controller = new AbortController()

    const queued = slots.acquire(controller.signal)
    controller.abort(new Error('caller went away'))

    await expect(queued).rejects.toBeInstanceOf(RunAbortedError)
  })

  test('an aborted waiter leaves the queue, so the slot goes to the next caller', async () => {
    const slots = new SlotPool(1, 1000)
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
    const slots = new SlotPool(1, 1000)
    await slots.acquire()
    const controller = new AbortController()

    const queued = slots.acquire(controller.signal)
    expect(slots.queueDepth).toBe(1)

    controller.abort()
    await expect(queued).rejects.toBeInstanceOf(RunAbortedError)
    expect(slots.queueDepth).toBe(0)
  })

  test('dispose rejects queued callers and refuses new ones', async () => {
    const slots = new SlotPool(1, 1000)
    await slots.acquire()
    const queued = slots.acquire()

    slots.dispose()
    await expect(queued).rejects.toThrow(/runtime disposed/)
    await expect(slots.acquire()).rejects.toThrow(/runtime is disposed/)
  })

  test('sheds callers past the queue bound with a QueueFullError', async () => {
    const slots = new SlotPool(1, 2)
    await slots.acquire()
    const q1 = slots.acquire()
    const q2 = slots.acquire()
    expect(slots.queueDepth).toBe(2)

    // The bound: the third waiter is shed immediately, queue untouched.
    await expect(slots.acquire()).rejects.toBeInstanceOf(QueueFullError)
    await expect(slots.acquire()).rejects.toThrow(/maxQueuedRuns: 2/)
    expect(slots.queueDepth).toBe(2)

    // Shedding is depth-based, not permanent: a freed slot drains the
    // queue and admission resumes.
    slots.release()
    await q1
    expect(slots.queueDepth).toBe(1)
    const q3 = slots.acquire()
    expect(slots.queueDepth).toBe(2)
    slots.release()
    await q2
    slots.release()
    await q3
  })

  test('a zero queue bound fails every caller past the slots immediately', async () => {
    const slots = new SlotPool(1, 0)
    await slots.acquire()
    await expect(slots.acquire()).rejects.toBeInstanceOf(QueueFullError)
    slots.release()
    await slots.acquire() // free slot: admitted without queueing
  })
})

describe('ConnectionRegistry', () => {
  test('shares one connection up to the run cap, then reports full', async () => {
    const shared = fakeClient()
    const connect = vi.fn().mockResolvedValue(shared)
    const registry = new ConnectionRegistry(connect)
    expect(registry.openConnections).toBe(0)
    expect(registry.tryAcquire()).toBeUndefined()

    await expect(registry.open()).resolves.toBe(shared)
    expect(registry.openConnections).toBe(1)

    // Below the cap the same connection serves every caller.
    for (let load = 0; load < RUNS_PER_CONNECTION; load++) {
      shared.load = load
      expect(registry.tryAcquire()).toBe(shared)
    }

    // At the cap the registry reports full — the caller opens another.
    shared.load = RUNS_PER_CONNECTION
    expect(registry.tryAcquire()).toBeUndefined()
    expect(connect).toHaveBeenCalledTimes(1)
  })

  test('counts a connect in flight, and a failed one leaves the ledger intact', async () => {
    const fresh = fakeClient()
    let settle: (c: RuntimeIpcClient) => void = () => {}
    const connect = vi.fn()
      .mockRejectedValueOnce(new Error('ECONNREFUSED'))
      .mockImplementationOnce(() => new Promise<RuntimeIpcClient>((r) => {
        settle = r
      }))
    const registry = new ConnectionRegistry(connect)

    await expect(registry.open()).rejects.toThrow(/ECONNREFUSED/)
    expect(registry.openConnections).toBe(0)

    const opening = registry.open()
    expect(registry.openConnections).toBe(1) // in flight, already visible
    settle(fresh)
    await expect(opening).resolves.toBe(fresh)
    expect(registry.openConnections).toBe(1)
  })

  test('drops a connection observed dead; the caller opens a replacement', async () => {
    const dying = fakeClient(true)
    const connect = vi.fn().mockResolvedValueOnce(dying)
    const registry = new ConnectionRegistry(connect)

    await registry.open()
    dying.usable = false

    expect(registry.tryAcquire()).toBeUndefined()
    expect(dying.dispose).toHaveBeenCalled()
    expect(registry.openConnections).toBe(0)
  })

  test('dispose closes tracked connections and refuses new callers', async () => {
    const open = fakeClient()
    const registry = new ConnectionRegistry(vi.fn().mockResolvedValue(open))
    await registry.open()

    await registry.dispose()
    expect(open.dispose).toHaveBeenCalled()
    expect(() => registry.tryAcquire()).toThrow(/runtime is disposed/)
    await expect(registry.open()).rejects.toThrow(/runtime is disposed/)
  })

  test('a connect resolving after dispose is closed, not tracked', async () => {
    let settle: (c: RuntimeIpcClient) => void = () => {}
    const late = fakeClient()
    const registry = new ConnectionRegistry(vi.fn().mockImplementation(
      () => new Promise<RuntimeIpcClient>((r) => {
        settle = r
      }),
    ))

    const opening = registry.open()
    await registry.dispose()
    settle(late)
    await expect(opening).rejects.toThrow(/runtime is disposed/)
    expect(late.dispose).toHaveBeenCalled()
  })
})

describe('RunPool composition', () => {
  test('never runs more than the slot capacity, even when acquires race', async () => {
    const pool = new RunPool(2, 1000, async () => fakeClient())

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

  test('concurrent runs below the cap share one connection', async () => {
    // The fakes never report load, so every run packs onto the first
    // connection — multiplexing, not one socket per run.
    const connect = vi.fn(async () => fakeClient())
    const pool = new RunPool(2, 1000, connect)
    expect(pool.openConnections).toBe(0)

    const first = hold(pool)
    await first.started
    const second = hold(pool)
    await second.started
    expect(pool.openConnections).toBe(1)

    first.release()
    second.release()
    await first.done
    await second.done

    // Kept for the process lifetime; a later run reuses it.
    await pool.withClient(async () => {})
    expect(connect).toHaveBeenCalledTimes(1)
    expect(pool.openConnections).toBe(1)
  })

  test('a connection at the run cap sends the next run to a fresh one', async () => {
    const full = fakeClient()
    const fresh = fakeClient()
    const connect = vi.fn()
      .mockResolvedValueOnce(full)
      .mockResolvedValueOnce(fresh)
    const pool = new RunPool(8, 1000, connect)

    const seen: RuntimeIpcClient[] = []
    await pool.withClient(async (c) => {
      seen.push(c)
      // The first connection reports itself at the cap from now on.
      full.load = RUNS_PER_CONNECTION
    })
    await pool.withClient(async (c) => {
      seen.push(c)
    })

    expect(seen).toEqual([full, fresh])
    expect(pool.openConnections).toBe(2)
  })

  test('a connect failure fails that run and frees its slot for the next', async () => {
    const fresh = fakeClient()
    const connect = vi.fn()
      .mockRejectedValueOnce(new Error('child is gone'))
      .mockResolvedValueOnce(fresh)
    const pool = new RunPool(1, 1000, connect)

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
    const pool = new RunPool(1, 1000, connect)

    const held = hold(pool, () => {
      dying.usable = false
    })
    await held.started

    const queued = pool.withClient(async (c) => c)
    expect(pool.queueDepth).toBe(1)

    held.release()
    await held.done
    await expect(queued).resolves.toBe(fresh)
    expect(dying.dispose).toHaveBeenCalled()
  })

  test('the slot frees at the Result: grace-phase work admits the next caller', async () => {
    // A run whose Result reported pending waitUntil work settles its caller
    // (fn returns) while the run keeps going runtime-side. The slot must
    // free right there — admission caps foreground execution, and the grace
    // frames ride the shared connection without holding capacity (#127).
    const client = fakeClient()
    const pool = new RunPool(1, 1000, vi.fn().mockResolvedValue(client))

    // Simulate the grace phase: the run is still routed on the connection
    // after its caller resolved.
    await pool.withClient(async () => {
      client.load = 1
    })
    const queued = pool.withClient(async (c) => c)
    await expect(queued).resolves.toBe(client)
    expect(pool.queueDepth).toBe(0)
  })

  test('dispose rejects queued callers', async () => {
    const pool = new RunPool(1, 1000, async () => fakeClient())
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
