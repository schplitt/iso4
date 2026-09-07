/**
 * Host-side codec for host types — `Headers`, `Request`, `Response`.
 *
 * Wire format: `docs/protocol.md` §4.4. The Rust counterpart is
 * `native/v8-runtime/src/webcodec.rs`.
 *
 * ## The two directions use different mechanisms
 *
 * **Reading** (sandbox → host) is a real V8 hook: `v8.Deserializer` exposes
 * `_readHostObject`, so a host object arriving from the sandbox is dispatched by
 * tag at any depth.
 *
 * **Writing** (host → sandbox) has no hook at all. Node's `v8.Serializer`
 * exposes no delegate to JavaScript and `_writeHostObject` never fires for a
 * class instance — the object silently flattens. So we do not try: each
 * instance is replaced by a **branded plain object** carrying the same fields,
 * the graph is serialized normally, and the runtime walks the result swapping
 * brands for real instances. That works at any depth, for any type, and needs no
 * hand-written framing.
 *
 * The asymmetry is safe because the two directions never share a reader.
 *
 * ## Bodies are async
 *
 * Reading a body off a host `Request`/`Response` is asynchronous, so the
 * transform is async: {@link materializeHostTypes} drains bodies and returns a
 * plain graph that ordinary synchronous serialization can then write.
 */

import { Buffer } from 'node:buffer'
import v8 from 'node:v8'
import { STREAM_PROBE_BYTES } from './ipc.js'

// ── Type tags (frozen — docs/protocol.md §4.4.1) ─────────────────────────────

export const TAG_INVALID = 0
export const TAG_HEADERS = 1
export const TAG_REQUEST = 2
export const TAG_RESPONSE = 3
export const TAG_READABLE_STREAM = 4
export const TAG_WRITABLE_STREAM = 5
export const TAG_WEB_SOCKET = 6
export const TAG_ABORT_SIGNAL = 7

function tagName(tag: number): string {
  switch (tag) {
    case TAG_HEADERS: return 'Headers'
    case TAG_REQUEST: return 'Request'
    case TAG_RESPONSE: return 'Response'
    case TAG_READABLE_STREAM: return 'ReadableStream'
    case TAG_WRITABLE_STREAM: return 'WritableStream'
    case TAG_WEB_SOCKET: return 'WebSocket'
    case TAG_ABORT_SIGNAL: return 'AbortSignal'
    default: return `unknown type (tag ${tag})`
  }
}

/**
 * Mirrors `MAX_HEADER_ENTRIES` in `webcodec.rs`.
 */
const MAX_HEADER_ENTRIES = 1024

/**
 * Thrown when a value cannot cross in this position. Surfaces to callers as
 * `ERR_TYPE_NOT_SERIALIZABLE`.
 */
export class HostTypeError extends TypeError {
  override readonly name = 'HostTypeError'
}

/**
 * Prefix of the property name marking a branded descriptor — a plain object
 * standing in for a host type on the way into the sandbox, which the runtime
 * swaps for a real instance. The full brand key is this prefix followed by the
 * sandbox's random descriptor token in hex ({@link brandKeyForToken}), so the
 * runtime rehydrates only descriptors this host actually stamped: inbound
 * structured data that happens to carry a brand-shaped key passes through as
 * the plain data it is. Wire contract: kept in sync with `BRAND_PREFIX` in
 * `native/v8-runtime/src/webcodec.rs`.
 */
export const BRAND_PREFIX = '__iso4_ht_'

/**
 * Byte length of the descriptor token carried in the `Authenticate` frame.
 * Mirrors `DESCRIPTOR_TOKEN_LEN` in `webcodec.rs`.
 */
export const DESCRIPTOR_TOKEN_LEN = 16

/**
 * Build the session brand key for a descriptor token.
 * @param token the sandbox's random descriptor token
 */
export function brandKeyForToken(token: Uint8Array): string {
  return BRAND_PREFIX + Buffer.from(token.buffer, token.byteOffset, token.byteLength).toString('hex')
}

// ── Streamed bodies (host → sandbox) ─────────────────────────────────────────

/**
 * One registered body source: the reader being pumped, chunks already read
 * during the probe (sent first), and the flow-control state the client's
 * pump loop maintains.
 */
export interface StreamSource {
  reader: ReadableStreamDefaultReader<Uint8Array>
  /**
   * Chunks read while probing (and oversized-chunk remainders), pumped before
   * the reader is pulled again.
   */
  prefix: Uint8Array[]
  /**
   * Bytes the runtime currently allows in flight. Replenished by
   * `StreamPull` frames as the sandbox consumes.
   */
  credit: number
  pumping: boolean
  done: boolean
}

/**
 * The per-run registry of body sources being streamed to the sandbox. Created
 * by the run entry points, filled by {@link materializeHostTypes} when a body
 * outgrows the probe, pumped by the client's frame loop.
 */
export class StreamSourceRegistry {
  private nextId = 1
  readonly sources: Map<number, StreamSource> = new Map()
  private readonly newIds: number[] = []

  /**
   * Register a source and return its stream id.
   * @param reader the body reader, positioned after the probe
   * @param prefix probed chunks to send first
   */
  register(reader: ReadableStreamDefaultReader<Uint8Array>, prefix: Uint8Array[]): number {
    const id = this.nextId++
    this.sources.set(id, { reader, prefix, credit: 0, pumping: false, done: false })
    this.newIds.push(id)
    return id
  }

  /**
   * Stream ids registered since the last take — the client activates their
   * pumps once the frame carrying the descriptors has been written.
   */
  takeNewIds(): number[] {
    return this.newIds.splice(0)
  }

  /**
   * Release every remaining source (run over, or torn down).
   */
  releaseAll(): void {
    for (const source of this.sources.values()) {
      source.done = true
      source.reader.cancel().catch(() => {})
    }
    this.sources.clear()
    this.newIds.length = 0
  }
}

// ── Outbound streams (sandbox → host, #128) ──────────────────────────────────

/**
 * How the registry talks back to the runtime: credit grants as the consumer
 * reads, cancels when it drops the body. Injected by the client (the frame
 * writer lives there).
 */
export interface OutboundStreamTransport {
  pull: (streamId: number, credit: number) => void
  cancel: (streamId: number, reason: string) => void
}

interface OutboundState {
  buffered: Uint8Array[]
  ended?: { error?: string }
  cancelled: boolean
  failed?: Error
  /**
   * Wakes the pending consumer pull when a chunk/end/failure arrives.
   */
  waiter?: () => void
}

/**
 * Per-run registry of OUTBOUND streamed bodies — result bodies the sandbox
 * produces and the consumer reads as a `ReadableStream`. Chunks arriving
 * before the Result is decoded (the runtime's eager initial window) buffer
 * here, bounded by the credit window; the consumer's reads replenish the
 * runtime's credit chunk by chunk, so a slow reader stops production at
 * the guest source.
 */
export class OutboundStreamRegistry {
  private readonly states = new Map<number, OutboundState>()
  private readonly transport: OutboundStreamTransport

  constructor(transport: OutboundStreamTransport) {
    this.transport = transport
  }

  private state(streamId: number): OutboundState {
    let st = this.states.get(streamId)
    if (st === undefined) {
      st = { buffered: [], cancelled: false }
      this.states.set(streamId, st)
    }
    return st
  }

  /**
   * A `StreamChunk` frame arrived for this run.
   * @param streamId
   * @param data
   */
  deliverChunk(streamId: number, data: Uint8Array): void {
    const st = this.state(streamId)
    if (st.cancelled)
      return
    st.buffered.push(data)
    st.waiter?.()
  }

  /**
   * A `StreamEnd` frame arrived: clean EOF, or a guest source failure.
   * @param streamId
   * @param error
   */
  deliverEnd(streamId: number, error?: string): void {
    const st = this.state(streamId)
    st.ended = error === undefined ? {} : { error }
    st.waiter?.()
  }

  /**
   * Connection-level failure (teardown, desync) or the run ending with a
   * stream never terminated: error every stream that has no outcome yet,
   * so no consumer read hangs.
   * @param error
   */
  failAll(error: Error): void {
    for (const st of this.states.values()) {
      if (st.ended === undefined && !st.cancelled && st.failed === undefined) {
        st.failed = error
        st.waiter?.()
      }
    }
  }

  /**
   * Hydrate one stream handle into the consumer-facing `ReadableStream`.
   * Called during Result decode (via the body-stream binder).
   * @param streamId
   */
  attach(streamId: number): ReadableStream<Uint8Array> {
    const st = this.state(streamId)
    const serve = (controller: ReadableStreamDefaultController<Uint8Array>): void | Promise<void> => {
      const chunk = st.buffered.shift()
      if (chunk !== undefined) {
        controller.enqueue(chunk)
        // Replenish exactly what the consumer took — the runtime's credit
        // ledger mirrors the inbound protocol with the roles swapped.
        this.transport.pull(streamId, chunk.byteLength)
        return
      }
      if (st.failed !== undefined) {
        controller.error(st.failed)
        return
      }
      if (st.ended !== undefined) {
        if (st.ended.error !== undefined) {
          controller.error(new Error(`[iso4] body stream failed: ${st.ended.error}`))
        } else {
          controller.close()
        }
        return
      }
      return new Promise<void>((resolve) => {
        st.waiter = () => {
          st.waiter = undefined
          resolve()
        }
      }).then(() => serve(controller))
    }
    return new ReadableStream<Uint8Array>({
      pull: (controller) => serve(controller),
      cancel: (reason) => {
        st.cancelled = true
        st.buffered = []
        if (st.ended === undefined && st.failed === undefined)
          this.transport.cancel(streamId, String(reason ?? 'cancelled'))
      },
    })
  }
}

// ── Materialized form ────────────────────────────────────────────────────────

/**
 * A body reduced to something synchronously writable — or a handle to a
 * registered stream when the body outgrew the probe. V8 preserves which of
 * the three inline shapes it is, so no kind discriminator travels on the
 * wire; the stream handle travels as the descriptor's `bodyStream` field.
 */
type MaterializedBody = Uint8Array | string | null | { streamId: number }

/**
 * A host type flattened into plain data. The session brand key (a dynamic
 * property name — see {@link brandKeyForToken}) holds the numeric type tag.
 *
 * Header entries are flattened to `[name, value, name, value, …]` — the shape
 * the sandbox keeps internally, so the runtime does not rebuild it. Never a
 * `Record`: a record cannot represent duplicate `set-cookie`.
 */
interface Descriptor {
  [brandKey: string]: unknown
  url?: string
  method?: string
  status?: number
  statusText?: string
  headers: string[]
  body: Uint8Array | string | null
  /**
   * Stream handle when the body outgrew the probe: the bytes follow as
   * `StreamChunk` frames under flow control, and the sandbox's `body`
   * becomes a socket-backed stream.
   */
  bodyStream?: number
}

function flatHeaders(headers: Headers): string[] {
  const out: string[] = []
  // `getSetCookie` is the only way to recover duplicates; `forEach` folds them
  // into one comma-joined value, which is wrong for cookies.
  const setCookies = typeof headers.getSetCookie === 'function' ? headers.getSetCookie() : []
  headers.forEach((value, name) => {
    if (name === 'set-cookie' && setCookies.length > 0)
      return
    out.push(name, value)
  })
  for (const cookie of setCookies) out.push('set-cookie', cookie)
  if (out.length / 2 > MAX_HEADER_ENTRIES) {
    throw new HostTypeError(
      `[iso4] Headers has ${out.length / 2} entries, exceeding the ${MAX_HEADER_ENTRIES} limit`,
    )
  }
  return out
}

/**
 * Ceiling on a host-supplied body, matching the default `maxBridgeCallBytes` /
 * `maxExportBytes`. The frame layer enforces the configured limit downstream;
 * this exists so an unbounded body cannot exhaust host memory *before* the
 * payload is ever framed.
 */
const MAX_BODY_BYTES = 16 * 1024 * 1024

async function materializeBody(
  source: Request | Response,
  streams?: StreamSourceRegistry,
): Promise<MaterializedBody> {
  // Serializing reads (and so consumes) the body; a second delivery of the
  // same instance has nothing left to read.
  if (source.bodyUsed) {
    throw new HostTypeError(
      '[iso4] cannot serialize a Request/Response whose body was already read — '
      + 'pass a fresh instance (cache the bytes, not the object)',
    )
  }

  const stream = source.body as ReadableStream<Uint8Array> | null
  if (stream === null || stream === undefined) {
    const buffer = await source.arrayBuffer()
    return buffer.byteLength === 0 ? null : new Uint8Array(buffer)
  }

  // Probe first: bodies that end within the probe take the buffered path
  // exactly as before (the common case — in Node even `new Response('text')`
  // is a ReadableStream, so shape alone cannot pick). On legs with a stream
  // registry, a body that outgrows the probe is registered for streaming
  // instead of being buffered whole; on legs without one (data globals,
  // whose values replay per instance), the historical full drain and its
  // byte cap apply.
  const probeLimit = streams === undefined ? MAX_BODY_BYTES : STREAM_PROBE_BYTES
  const reader = stream.getReader()
  const chunks: Uint8Array[] = []
  let total = 0
  for (;;) {
    let result: Awaited<ReturnType<typeof reader.read>>
    try {
      result = await reader.read()
    } catch (error) {
      reader.releaseLock()
      throw error
    }
    if (result.done)
      break
    total += result.value.byteLength
    chunks.push(result.value)
    if (total > probeLimit) {
      if (streams !== undefined)
        return { streamId: streams.register(reader, chunks) }
      reader.releaseLock()
      throw new HostTypeError(
        `[iso4] body exceeds ${MAX_BODY_BYTES} bytes; a Request/Response crossing the `
        + 'boundary must be bounded — buffer and truncate it first',
      )
    }
  }
  reader.releaseLock()
  if (total === 0)
    return null
  const out = new Uint8Array(total)
  let offset = 0
  for (const chunk of chunks) {
    out.set(chunk, offset)
    offset += chunk.byteLength
  }
  return out
}

function bodyFields(body: MaterializedBody): { body: Uint8Array | string | null, bodyStream?: number } {
  if (body !== null && typeof body === 'object' && 'streamId' in body)
    return { body: null, bodyStream: body.streamId }
  return { body }
}

async function toDescriptor(
  value: object,
  brandKey: string,
  streams?: StreamSourceRegistry,
): Promise<Descriptor | undefined> {
  if (value instanceof globalThis.Response) {
    return {
      [brandKey]: TAG_RESPONSE,
      status: value.status,
      statusText: value.statusText,
      headers: flatHeaders(value.headers),
      ...bodyFields(await materializeBody(value, streams)),
    }
  }
  if (value instanceof globalThis.Request) {
    return {
      [brandKey]: TAG_REQUEST,
      url: value.url,
      method: value.method,
      headers: flatHeaders(value.headers),
      ...bodyFields(await materializeBody(value, streams)),
    }
  }
  if (value instanceof globalThis.Headers)
    return { [brandKey]: TAG_HEADERS, headers: flatHeaders(value), body: null }
  return undefined
}

/**
 * Depth cap, matching `MAX_DEPTH` in `webcodec.rs`. Cycles are handled by the
 * `seen` map, so this only bounds pathologically deep graphs.
 */
const MAX_DEPTH = 32

/**
 * Replace every `Request`/`Response`/`Headers` anywhere in `value` with a
 * branded plain object, returning a graph ordinary serialization can write.
 *
 * Structure is rebuilt rather than mutated, so the surrounding objects are
 * untouched. A `Request`/`Response` body, however, is read into bytes here and
 * is therefore consumed — one-shot, like any body read: pass an unread
 * instance and do not reuse it afterwards (streams cannot cross the boundary,
 * so the body must be buffered rather than forwarded). Object identity and
 * cycles are preserved through `seen`. `Map`/`Set` are rebuilt because a host
 * type can hide in either.
 * @param value the value to transform
 * @param brandKey the sandbox's session brand key ({@link brandKeyForToken});
 * the runtime rehydrates only descriptors stamped with it
 * @param streams
 */
export async function materializeHostTypes(
  value: unknown,
  brandKey: string,
  streams?: StreamSourceRegistry,
): Promise<unknown> {
  return transform(value, brandKey, streams, new Map(), 0)
}

async function transform(
  value: unknown,
  brandKey: string,
  streams: StreamSourceRegistry | undefined,
  seen: Map<object, unknown>,
  depth: number,
): Promise<unknown> {
  if (value === null || typeof value !== 'object')
    return value
  if (depth > MAX_DEPTH) {
    throw new HostTypeError(
      `[iso4] value nests deeper than ${MAX_DEPTH} levels; cannot scan it for host types`,
    )
  }

  const existing = seen.get(value)
  if (existing !== undefined)
    return existing

  const descriptor = await toDescriptor(value, brandKey, streams)
  if (descriptor !== undefined) {
    seen.set(value, descriptor)
    return descriptor
  }

  // Leave anything V8 carries natively alone — typed arrays, Date, RegExp,
  // ArrayBuffer. Rebuilding those would lose their type.
  if (ArrayBuffer.isView(value) || value instanceof ArrayBuffer
    || value instanceof Date || value instanceof RegExp || value instanceof Error) {
    return value
  }

  if (Array.isArray(value)) {
    const out: unknown[] = []
    seen.set(value, out)
    for (const item of value) out.push(await transform(item, brandKey, streams, seen, depth + 1))
    return out
  }

  if (value instanceof Map) {
    const out = new Map<unknown, unknown>()
    seen.set(value, out)
    for (const [k, v] of value)
      out.set(await transform(k, brandKey, streams, seen, depth + 1), await transform(v, brandKey, streams, seen, depth + 1))
    return out
  }

  if (value instanceof Set) {
    const out = new Set<unknown>()
    seen.set(value, out)
    for (const v of value) out.add(await transform(v, brandKey, streams, seen, depth + 1))
    return out
  }

  // A plain object, or a class instance that would flatten anyway (§4.2).
  // Null-prototype so an own `__proto__` key is copied as data instead of
  // hitting the `Object.prototype` `__proto__` setter (which would re-point the
  // prototype and drop the key from the serialized output). `ValueSerializer`
  // does not preserve the null prototype, so the sandbox still gets an ordinary
  // object.
  const out: Record<string, unknown> = Object.create(null) as Record<string, unknown>
  seen.set(value, out)
  for (const [k, v] of Object.entries(value))
    out[k] = await transform(v, brandKey, streams, seen, depth + 1)
  return out
}

// ── Reading ──────────────────────────────────────────────────────────────────

/**
 * The subset of `v8.Deserializer` used here that `@types/node` does not
 * declare. `readRawBytes` returns a view into the source buffer, not a copy.
 */
interface DeserializerInternals {
  readUint32: () => number
  readRawBytes: (length: number) => Buffer
  readValue: () => unknown
}

function readStr(des: DeserializerInternals): string {
  const len = des.readUint32()
  return des.readRawBytes(len).toString('utf8')
}

function readHeaders(des: DeserializerInternals): [string, string][] {
  const flat = des.readValue()
  if (!Array.isArray(flat) || flat.length % 2 !== 0)
    throw new HostTypeError('[iso4] Headers entry list is malformed')
  if (flat.length / 2 > MAX_HEADER_ENTRIES) {
    throw new HostTypeError(
      `[iso4] Headers payload declares ${flat.length / 2} entries, exceeding the ${MAX_HEADER_ENTRIES} limit`,
    )
  }
  const out: [string, string][] = []
  for (let i = 0; i < flat.length; i += 2)
    out.push([String(flat[i]), String(flat[i + 1])])
  return out
}

function readBody(des: DeserializerInternals): Uint8Array | string | null {
  const body = des.readValue()
  if (body === null || body === undefined)
    return null
  if (typeof body === 'string' || body instanceof Uint8Array)
    return body
  throw new HostTypeError(
    '[iso4] received a body that is neither bytes, a string, nor absent',
  )
}

/**
 * Binder resolving an outbound stream handle (from a descriptor's extras)
 * into the consumer-facing `ReadableStream`. Installed around a Result
 * decode by {@link withBodyStreamBinder}; module-level because the
 * deserializer hook chain has no context parameter — safe, the decode is
 * synchronous.
 */
let currentBodyStreamBinder: ((streamId: number) => ReadableStream<Uint8Array>) | undefined

/**
 * Run `fn` with `binder` resolving outbound body-stream handles.
 * @param binder
 * @param fn
 */
export function withBodyStreamBinder<T>(
  binder: (streamId: number) => ReadableStream<Uint8Array>,
  fn: () => T,
): T {
  currentBodyStreamBinder = binder
  try {
    return fn()
  } finally {
    currentBodyStreamBinder = undefined
  }
}

/**
 * Read the extras blob and resolve an outbound body-stream handle when one
 * rides in it (#128). Extras is a self-contained V8 blob holding a plain
 * object — the forward-compatible field slot (§4.4.4).
 * @param des
 */
function readExtras(des: DeserializerInternals): { bodyStream?: ReadableStream<Uint8Array> } {
  const len = des.readUint32()
  if (len === 0)
    return {}
  const bytes = des.readRawBytes(len)
  let extras: unknown
  try {
    extras = v8.deserialize(bytes)
  } catch {
    throw new HostTypeError('[iso4] descriptor extras blob is malformed')
  }
  const streamId = (extras !== null && typeof extras === 'object')
    ? (extras as Record<string, unknown>)['bodyStream']
    : undefined
  if (typeof streamId !== 'number')
    return {}
  if (currentBodyStreamBinder === undefined) {
    throw new HostTypeError(
      '[iso4] a stream body arrived on a leg that cannot carry one',
    )
  }
  return { bodyStream: currentBodyStreamBinder(streamId) }
}

/**
 * Methods that must not carry a body, per the fetch spec.
 */
const BODYLESS_METHODS = new Set(['GET', 'HEAD'])

/**
 * Read one host-object payload, tag first, and construct the configured class.
 *
 * Called from `_readHostObject`, which must return an object — Node throws
 * `readHostObject must return an object` otherwise.
 * @param des deserializer, positioned at the type tag
 */
export function readHostType(des: DeserializerInternals): object {
  const tag = des.readUint32()
  switch (tag) {
    case TAG_HEADERS:
      return new globalThis.Headers(readHeaders(des))

    case TAG_REQUEST: {
      const url = readStr(des)
      const method = readStr(des)
      const headers = readHeaders(des)
      const body = readBody(des)
      const { bodyStream } = readExtras(des)
      // Structural rather than the DOM `RequestInit`: this package targets
      // Node without the DOM lib, and only these fields are set. A stream
      // body needs `duplex` per Node's fetch rules.
      const init: {
        method: string
        headers: [string, string][]
        body?: Uint8Array | string | ReadableStream<Uint8Array>
        duplex?: 'half'
      } = { method, headers }
      if (!BODYLESS_METHODS.has(method.toUpperCase())) {
        if (bodyStream !== undefined) {
          init.body = bodyStream
          init.duplex = 'half'
        } else if (body !== null) {
          init.body = body
        }
      }
      return new globalThis.Request(url, init)
    }

    case TAG_RESPONSE: {
      const status = des.readUint32()
      const statusText = readStr(des)
      const headers = readHeaders(des)
      const body = readBody(des)
      const { bodyStream } = readExtras(des)
      // Status 0 means Response.error(); `new Response(null, {status: 0})`
      // throws in Node, so it has to be built the same way it was in the
      // sandbox. Headers stay an array of pairs rather than a Record — a Record
      // cannot represent duplicate set-cookie.
      if (status === 0)
        return globalThis.Response.error()
      return new globalThis.Response(bodyStream ?? body, { status, statusText, headers })
    }

    case TAG_INVALID:
      throw new HostTypeError('[iso4] payload carries the reserved invalid type tag')

    default:
      throw new HostTypeError(
        `[iso4] received a ${tagName(tag)}; this build cannot materialise one`,
      )
  }
}

/**
 * Deserializer that understands host objects.
 *
 * Extends `v8.Deserializer`, **not** `v8.DefaultDeserializer`: the latter's
 * `_readHostObject` decodes Node's private typed-array host-object format and
 * would misparse our tag. Safe because neither side ever writes that format —
 * `_setTreatArrayBufferViewsAsHostObjects(false)` is set on every serializer.
 */
export class HostTypeDeserializer extends v8.Deserializer {
  _readHostObject(): object {
    return readHostType(this as unknown as DeserializerInternals)
  }
}
