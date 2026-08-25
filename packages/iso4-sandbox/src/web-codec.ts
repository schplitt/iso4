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

import type { Buffer } from 'node:buffer'
import v8 from 'node:v8'

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
 * Property name marking a branded descriptor — a plain object standing in for a
 * host type on the way into the sandbox, which the runtime swaps for a real
 * instance. Wire contract: kept in sync with `BRAND` in
 * `native/v8-runtime/src/webcodec.rs`.
 */
export const BRAND = '__iso4_ht'

// ── Materialized form ────────────────────────────────────────────────────────

/**
 * A body reduced to something synchronously writable. V8 preserves which of
 * the three it is, so no kind discriminator travels on the wire.
 */
type MaterializedBody = Uint8Array | string | null

/**
 * A host type flattened into plain data.
 *
 * Header entries are flattened to `[name, value, name, value, …]` — the shape
 * the sandbox keeps internally, so the runtime does not rebuild it. Never a
 * `Record`: a record cannot represent duplicate `set-cookie`.
 */
interface Descriptor {
  [BRAND]: number
  url?: string
  method?: string
  status?: number
  statusText?: string
  headers: string[]
  body: MaterializedBody
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

async function materializeBody(source: Request | Response): Promise<MaterializedBody> {
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

  // Drained by hand rather than with `arrayBuffer()` so the byte count is
  // checked as it arrives. There is no way to tell a buffered body from a
  // genuinely streaming one — in Node both are a ReadableStream, including
  // `new Response('text')` — so refusing streams outright would refuse
  // everything. Capping the read is the mitigation that actually applies.
  const reader = stream.getReader()
  const chunks: Uint8Array[] = []
  let total = 0
  try {
    for (;;) {
      const { done, value } = await reader.read()
      if (done)
        break
      total += value.byteLength
      if (total > MAX_BODY_BYTES) {
        throw new HostTypeError(
          `[iso4] body exceeds ${MAX_BODY_BYTES} bytes; a Request/Response crossing the `
          + 'boundary must be bounded — buffer and truncate it first',
        )
      }
      chunks.push(value)
    }
  } finally {
    reader.releaseLock()
  }
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

async function toDescriptor(value: object): Promise<Descriptor | undefined> {
  if (value instanceof globalThis.Response) {
    return {
      [BRAND]: TAG_RESPONSE,
      status: value.status,
      statusText: value.statusText,
      headers: flatHeaders(value.headers),
      body: await materializeBody(value),
    }
  }
  if (value instanceof globalThis.Request) {
    return {
      [BRAND]: TAG_REQUEST,
      url: value.url,
      method: value.method,
      headers: flatHeaders(value.headers),
      body: await materializeBody(value),
    }
  }
  if (value instanceof globalThis.Headers)
    return { [BRAND]: TAG_HEADERS, headers: flatHeaders(value), body: null }
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
 */
export async function materializeHostTypes(value: unknown): Promise<unknown> {
  return transform(value, new Map(), 0)
}

async function transform(
  value: unknown,
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

  const descriptor = await toDescriptor(value)
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
    for (const item of value) out.push(await transform(item, seen, depth + 1))
    return out
  }

  if (value instanceof Map) {
    const out = new Map<unknown, unknown>()
    seen.set(value, out)
    for (const [k, v] of value)
      out.set(await transform(k, seen, depth + 1), await transform(v, seen, depth + 1))
    return out
  }

  if (value instanceof Set) {
    const out = new Set<unknown>()
    seen.set(value, out)
    for (const v of value) out.add(await transform(v, seen, depth + 1))
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
    out[k] = await transform(v, seen, depth + 1)
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

function skipExtras(des: DeserializerInternals): void {
  const len = des.readUint32()
  if (len > 0)
    des.readRawBytes(len)
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
      skipExtras(des)
      // Structural rather than the DOM `RequestInit`: this package targets
      // Node without the DOM lib, and only these three fields are set.
      const init: { method: string, headers: [string, string][], body?: Uint8Array | string }
        = { method, headers }
      if (body !== null && !BODYLESS_METHODS.has(method.toUpperCase()))
        init.body = body
      return new globalThis.Request(url, init)
    }

    case TAG_RESPONSE: {
      const status = des.readUint32()
      const statusText = readStr(des)
      const headers = readHeaders(des)
      const body = readBody(des)
      skipExtras(des)
      // Status 0 means Response.error(); `new Response(null, {status: 0})`
      // throws in Node, so it has to be built the same way it was in the
      // sandbox. Headers stay an array of pairs rather than a Record — a Record
      // cannot represent duplicate set-cookie.
      if (status === 0)
        return globalThis.Response.error()
      return new globalThis.Response(body, { status, statusText, headers })
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
