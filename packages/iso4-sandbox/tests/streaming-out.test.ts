/**
 * Outbound response-body streaming (#128), end to end: a sandbox call
 * returns a Response whose body is produced lazily (async generator) or
 * passed through from an inbound stream, and the host consumes it as a
 * real `ReadableStream` under credit-based flow control.
 */

import { afterAll, beforeAll, describe, expect, test } from 'vitest'
import type { Sandbox } from '../src/index.js'
import { createSandbox } from '../src/index.js'

function patternBytes(length: number): Uint8Array {
  const out = new Uint8Array(length)
  for (let i = 0; i < length; i++) out[i] = i % 251
  return out
}

describe('outbound streaming bodies', () => {
  let sandbox: Sandbox

  beforeAll(async () => {
    sandbox = await createSandbox({ maxConcurrentRuns: 4 })
  })

  afterAll(async () => {
    await sandbox?.dispose()
  })

  test('a generator body arrives as a real ReadableStream and drains fully', async () => {
    await using prefix = await sandbox.prepare({
      code: `
        export async function stream() {
          async function* body() {
            yield new Uint8Array([104, 101, 108])
            yield 'lo '
            yield new Uint8Array([119, 111, 114, 108, 100])
          }
          return new Response(body(), { status: 201, headers: { 'x-kind': 'streamed' } })
        }
      `,
    })
    const result = await prefix.call({ export: 'stream' })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    const response = result.value as Response
    expect(response).toBeInstanceOf(Response)
    expect(response.status).toBe(201)
    expect(response.headers.get('x-kind')).toBe('streamed')
    expect(response.body).toBeInstanceOf(ReadableStream)
    // A streaming-only epilogue is invisible: no waitUntil promise appears.
    expect(result.waitUntil).toBeUndefined()
    expect(await response.text()).toBe('hello world')
  })

  test('a multi-window body streams fully under credit (4 MiB)', async () => {
    const TOTAL = 4 * 1024 * 1024
    await using prefix = await sandbox.prepare({
      code: `
        export async function big(total) {
          async function* body() {
            let sent = 0
            while (sent < total) {
              const n = Math.min(65536, total - sent)
              const chunk = new Uint8Array(n)
              for (let i = 0; i < n; i++) chunk[i] = (sent + i) % 251
              sent += n
              yield chunk
            }
          }
          return new Response(body())
        }
      `,
    })
    const result = await prefix.call({ export: 'big', args: [TOTAL] })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    const bytes = new Uint8Array(await (result.value as Response).arrayBuffer())
    expect(bytes.byteLength).toBe(TOTAL)
    expect(bytes).toEqual(patternBytes(TOTAL))
  }, 30_000)

  test('pass-through: new Response(request.body) echoes an inbound stream', async () => {
    await using prefix = await sandbox.prepare({
      code: `
        export async function echo(req) {
          return new Response(req.body)
        }
      `,
    })
    const payload = patternBytes(512 * 1024) // over the inbound probe: streams in
    const result = await prefix.call({
      export: 'echo',
      args: [new Request('https://example.test/echo', { method: 'POST', body: payload, duplex: 'half' })],
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    const bytes = new Uint8Array(await (result.value as Response).arrayBuffer())
    expect(bytes.byteLength).toBe(payload.byteLength)
    expect(bytes).toEqual(payload)
  }, 30_000)

  test('cancelling the host reader runs the generator finally; instance stays warm', async () => {
    await using prefix = await sandbox.prepare({
      code: `
        let cleanedUp = 0
        export function probe() { return cleanedUp }
        export async function hang() {
          async function* body() {
            try {
              yield new Uint8Array(300000)
              yield new Uint8Array(1)
            } finally {
              cleanedUp = 1
            }
          }
          return new Response(body())
        }
      `,
    })
    // Seed the prefix's demand averages so the probe below JOINS the
    // instance holding the stream instead of spawning a fresh one (whose
    // module state would read 0).
    await prefix.call({ export: 'probe' })
    await prefix.call({ export: 'probe' })

    const result = await prefix.call({ export: 'hang' })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    const body = (result.value as Response).body!
    const reader = body.getReader()
    const first = await reader.read()
    expect(first.done).toBe(false)
    await reader.cancel('lost interest')

    // web-streams semantics: the generator's finally ran in the sandbox,
    // and the warm instance keeps serving.
    await expect.poll(async () => {
      const after = await prefix.call({ export: 'probe' })
      return after.ok ? after.value : undefined
    }).toBe(1)
  })

  test('a guest source failure rejects the read, not the run', async () => {
    await using prefix = await sandbox.prepare({
      code: `
        export async function broken() {
          async function* body() {
            yield new Uint8Array([1])
            throw new Error('source exploded')
          }
          return new Response(body())
        }
      `,
    })
    const result = await prefix.call({ export: 'broken' })
    // The run completed: the Result shipped with the handle; the failure is
    // a body event, like a fetch body dying mid-transfer.
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    await expect((result.value as Response).arrayBuffer()).rejects.toThrow('source exploded')
  })

  test('waitUntil and a streamed body share the epilogue', async () => {
    await using prefix = await sandbox.prepare({
      code: `
        import { waitUntil } from 'iso4:runtime'
        export async function both() {
          waitUntil(Promise.resolve().then(() => { globalThis.__done = true }))
          async function* body() { yield 'streamed alongside grace' }
          return new Response(body())
        }
      `,
    })
    const result = await prefix.call({ export: 'both', limits: { graceMs: 1_000 } })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(await (result.value as Response).text()).toBe('streamed alongside grace')
    expect(result.waitUntil).toBeDefined()
    const report = await result.waitUntil!
    expect(report.status).toBe('settled')
  })

  test('one-off run({ call }) can stream a result body too', async () => {
    const result = await sandbox.run({
      code: `
        export async function stream() {
          async function* body() { yield 'one'; yield '-off' }
          return new Response(body())
        }
      `,
      call: { export: 'stream' },
    })
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(await (result.value as Response).text()).toBe('one-off')
  })
})
