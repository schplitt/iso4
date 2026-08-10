/**
 * Host-side value codec — V8 serialization blobs ("v8 blob").
 *
 * Every value crossing the Node ↔ Rust boundary travels as a blob produced by
 * V8's own `ValueSerializer` (`docs/protocol.md` §4). The Rust side uses
 * `v8::ValueSerializer` / `v8::ValueDeserializer` against the identical byte
 * format, so no hand-written codec sits between the two V8s.
 *
 * Two details are load-bearing and must not be "simplified":
 *
 * - `_setTreatArrayBufferViewsAsHostObjects(false)` is **mandatory**. Node's
 *   default `v8.serialize()` writes typed arrays with a Node-private
 *   host-object tag that a plain (non-Node) V8 rejects at read time.
 * - `writeHeader()` must precede `writeValue()`; the header carries the format
 *   version the reader validates.
 */

import type { Buffer } from 'node:buffer'
import v8 from 'node:v8'

/**
 * The subset of `v8.Serializer` we need that `@types/node` does not declare.
 */
interface SerializerInternals {
  _setTreatArrayBufferViewsAsHostObjects: (flag: boolean) => void
}

/**
 * Thrown when a value cannot be represented in the V8 serialization format.
 *
 * V8 raises a `DataCloneError`-style exception for functions, symbols,
 * promises, `WeakMap`/`WeakSet`, and proxies. Everything else — including
 * `Date`, `Map`, `Set`, `RegExp`, `Error`, `ArrayBuffer`, every `TypedArray`,
 * `bigint`, and cyclic graphs — round-trips as a real instance.
 *
 * Internal: never surfaces through the public API. Callers translate it into
 * the typed error the boundary expects (`ERR_HOST_BRIDGE` for bridge
 * responses, a rejected data global at encode time).
 */
export class ValueEncodeError extends TypeError {
  override readonly name = 'ValueEncodeError'
}

/**
 * Thrown when a blob cannot be read back — a truncated payload, or a blob
 * written by a V8 whose serialization format version this Node cannot read.
 *
 * The format version is checked once per connection at handshake time
 * (see `client.ts`), so reaching this at run time means a corrupt payload.
 */
export class ValueDecodeError extends TypeError {
  override readonly name = 'ValueDecodeError'
}

/**
 * Serialize one JavaScript value into a V8 serialization blob.
 * @param value the value to encode
 * @throws {ValueEncodeError} when V8 refuses to clone the value
 */
export function serializeValue(value: unknown): Buffer {
  const serializer = new v8.DefaultSerializer()
  // MANDATORY — see the module docblock.
  ;(serializer as unknown as SerializerInternals)
    ._setTreatArrayBufferViewsAsHostObjects(false)
  serializer.writeHeader()
  try {
    serializer.writeValue(value)
  } catch (error) {
    throw new ValueEncodeError(
      `[iso4] cannot serialize value: ${error instanceof Error ? error.message : String(error)}`,
    )
  }
  return serializer.releaseBuffer()
}

/**
 * Read one JavaScript value back from a V8 serialization blob.
 * @param bytes the blob, exactly as produced by {@link serializeValue} or by
 * the Rust `blob::serialize_value`
 * @throws {ValueDecodeError} on a truncated, corrupt, or newer-format blob
 */
export function deserializeValue(bytes: Uint8Array): unknown {
  try {
    return v8.deserialize(bytes)
  } catch (error) {
    throw new ValueDecodeError(
      `[iso4] cannot deserialize value: ${error instanceof Error ? error.message : String(error)}`,
    )
  }
}

/**
 * The handshake probe: a serialized `null`.
 *
 * Byte 0 is V8's header tag (`0xFF`) and byte 1 is the **format version** this
 * Node writes. Each side sends its probe at connection setup so a format-
 * version mismatch fails loudly at `createSandbox()` instead of corrupting a
 * value mid-run. See `docs/protocol.md` §5.1.
 */
export function serializationProbe(): Buffer {
  return serializeValue(null)
}

/**
 * V8 serialization header tag — the first byte of every blob.
 */
export const V8_BLOB_HEADER_TAG = 0xFF

/**
 * Read the format version out of a probe blob, or `undefined` when the bytes
 * are not a V8 serialization blob at all.
 * @param probe the probe bytes received from the peer
 */
export function probeFormatVersion(probe: Uint8Array): number | undefined {
  if (probe.length < 2 || probe[0] !== V8_BLOB_HEADER_TAG)
    return undefined
  return probe[1]
}
