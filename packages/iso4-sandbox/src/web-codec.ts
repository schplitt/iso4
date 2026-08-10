/**
 * Host-side codec for host types — `Headers`, `Request`, `Response`.
 *
 * Wire format: `docs/protocol.md` §4.4. The Rust counterpart is
 * `native/v8-runtime/src/webcodec.rs`; the two must agree byte for byte.
 *
 * ## Why the two directions look different
 *
 * Reading is a real V8 hook: `v8.Deserializer` exposes `_readHostObject`, so a
 * host object arriving from the sandbox is dispatched by tag at any depth.
 *
 * Writing has no hook. Node's `v8.Serializer` gives JavaScript no delegate, and
 * `_writeHostObject` never fires for a class instance — the object silently
 * flattens to its own enumerable properties. So the payload is emitted **by
 * hand**: the `kHostObject` tag byte followed by the same bytes Rust would
 * write. That only works where we control emission, which is the top level of a
 * value slot. See §4.4.6.
 *
 * ## Bodies are async
 *
 * Reading a body off a host `Request`/`Response` is asynchronous, so encoding
 * splits in two: {@link materializeHostType} drains the body and returns a
 * plain snapshot, then {@link writeHostType} emits it synchronously.
 */

import type { Buffer } from 'node:buffer'
import { Buffer as NodeBuffer } from 'node:buffer'
import v8 from 'node:v8'

/**
 * V8's `kHostObject` tag — the byte that introduces an embedder payload where a
 * value is expected. Both V8s agree on it because the handshake pins the
 * serialization format version (`docs/protocol.md` §5.1).
 */
const K_HOST_OBJECT = 0x5C

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

// ── Materialized form ────────────────────────────────────────────────────────

/**
 * A body reduced to something synchronously writable. V8 preserves which of
 * the three it is, so no kind discriminator travels on the wire.
 */
type MaterializedBody = Uint8Array | string | null

/**
 * A host type flattened into plain data, ready for synchronous emission.
 * Header entries stay an array of pairs, never a `Record` — a record cannot
 * represent duplicate `set-cookie`.
 */
/**
 * Marks a {@link MaterializedHostType} so a synchronous encoder can recognise
 * one that an earlier async pass produced. `Symbol.for` rather than a private
 * symbol so duplicate copies of this module in a dependency tree still agree.
 */
// `unique symbol`, not `symbol`: the interface below uses it as a computed
// property key, and `isolatedDeclarations` needs the annotation to be explicit.
const MATERIALIZED: unique symbol = Symbol.for('iso4.materializedHostType')

export function isMaterializedHostType(value: unknown): value is MaterializedHostType {
  return typeof value === 'object' && value !== null
    && (value as Record<symbol, unknown>)[MATERIALIZED] === true
}

export interface MaterializedHostType {
  [MATERIALIZED]?: true
  tag: number
  url?: string
  method?: string
  status?: number
  statusText?: string
  headers: [string, string][]
  body: MaterializedBody
}

function headerPairs(headers: Headers): [string, string][] {
  const out: [string, string][] = []
  // `getSetCookie` is the only way to recover duplicates; `forEach` folds them
  // into one comma-joined value, which is wrong for cookies.
  const setCookies = typeof headers.getSetCookie === 'function' ? headers.getSetCookie() : []
  headers.forEach((value, name) => {
    if (name === 'set-cookie' && setCookies.length > 0)
      return
    out.push([name, value])
  })
  for (const cookie of setCookies) out.push(['set-cookie', cookie])
  return out
}

async function materializeBody(source: Request | Response): Promise<MaterializedBody> {
  // `bodyUsed` would make the read throw; report it as the caller's mistake.
  if (source.bodyUsed)
    throw new HostTypeError('[iso4] cannot serialize a Request/Response whose body was already read')
  const buffer = await source.arrayBuffer()
  return buffer.byteLength === 0 ? null : new Uint8Array(buffer)
}

/**
 * Recognise and flatten a host type. Returns `undefined` for anything else.
 *
 * Async because bodies are. Callers on a synchronous path must handle
 * `undefined` and fall through to ordinary value serialization.
 * @param value the candidate host type
 */
export async function materializeHostType(
  value: unknown,
): Promise<MaterializedHostType | undefined> {
  if (value === null || typeof value !== 'object')
    return undefined

  if (value instanceof globalThis.Response) {
    const res = value as Response
    return {
      [MATERIALIZED]: true,
      tag: TAG_RESPONSE,
      status: res.status,
      statusText: res.statusText,
      headers: headerPairs(res.headers),
      body: await materializeBody(res),
    }
  }
  if (value instanceof globalThis.Request) {
    const req = value as Request
    return {
      [MATERIALIZED]: true,
      tag: TAG_REQUEST,
      url: req.url,
      method: req.method,
      headers: headerPairs(req.headers),
      body: await materializeBody(req),
    }
  }
  if (value instanceof globalThis.Headers) {
    return {
      [MATERIALIZED]: true,
      tag: TAG_HEADERS,
      headers: headerPairs(value as Headers),
      body: null,
    }
  }
  return undefined
}

// ── Writing ──────────────────────────────────────────────────────────────────

/**
 * The subset of `v8.Serializer` used here that `@types/node` does not declare.
 */
interface SerializerInternals {
  writeRawBytes: (bytes: Uint8Array) => void
  writeUint32: (value: number) => void
  writeValue: (value: unknown) => void
  _setTreatArrayBufferViewsAsHostObjects: (flag: boolean) => void
}

function writeStr(ser: SerializerInternals, text: string): void {
  const bytes = NodeBuffer.from(text, 'utf8')
  ser.writeUint32(bytes.byteLength)
  ser.writeRawBytes(bytes)
}

/**
 * Headers travel as one flat `[name, value, name, value, …]` array — the same
 * shape the sandbox keeps internally, so neither side rebuilds it element by
 * element across the V8 boundary.
 * @param ser the serializer
 * @param entries header pairs
 */
function writeHeaders(ser: SerializerInternals, entries: [string, string][]): void {
  if (entries.length > MAX_HEADER_ENTRIES) {
    throw new HostTypeError(
      `[iso4] Headers has ${entries.length} entries, exceeding the ${MAX_HEADER_ENTRIES} limit`,
    )
  }
  const flat: string[] = []
  for (const [rawName, value] of entries) flat.push(rawName.toLowerCase(), value)
  ser.writeValue(flat)
}

function writeBody(ser: SerializerInternals, body: MaterializedBody): void {
  // `_setTreatArrayBufferViewsAsHostObjects(false)` on every serializer means a
  // Uint8Array here is written in plain V8 form, which the runtime can read.
  ser.writeValue(body)
}

/**
 * Emit a host-object payload by hand: the `kHostObject` tag, the type tag, then
 * the body. Must be called where a value is expected in the byte stream.
 * @param ser serializer, positioned where a value is expected
 * @param m the host type, already drained by {@link materializeHostType}
 */
export function writeHostType(ser: SerializerInternals, m: MaterializedHostType): void {
  ser.writeRawBytes(NodeBuffer.from([K_HOST_OBJECT]))
  ser.writeUint32(m.tag)
  switch (m.tag) {
    case TAG_HEADERS:
      writeHeaders(ser, m.headers)
      return
    case TAG_REQUEST:
      writeStr(ser, m.url ?? '')
      writeStr(ser, m.method ?? 'GET')
      writeHeaders(ser, m.headers)
      writeBody(ser, m.body)
      ser.writeUint32(0) // extras: absent
      return
    case TAG_RESPONSE:
      ser.writeUint32(m.status ?? 200)
      writeStr(ser, m.statusText ?? '')
      writeHeaders(ser, m.headers)
      writeBody(ser, m.body)
      ser.writeUint32(0) // extras: absent
      return
    default:
      throw new HostTypeError(`[iso4] cannot serialize ${tagName(m.tag)}`)
  }
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
      // `headers` stays an array of pairs rather than a Record: a Record
      // cannot represent duplicate set-cookie.
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
