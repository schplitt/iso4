/**
 * Host-side wire codec microbenchmarks — `encodeWireValue` /
 * `decodeWireValue` per payload shape, isolated from socket and V8 costs.
 *
 * The Rust legs of the same codec (plus the V8-internal serializers this
 * codec is compared against) live in `native/v8-runtime/benches/micro.rs`.
 */

import { bench, describe } from 'vitest'
import { decodeWireValue, encodeWireValue } from '../src/wire.js'
import { PAYLOAD_MATRIX } from './payloads.js'
import { MICRO_OPTS } from './profile.js'

describe('codec encodeWireValue', () => {
  for (const { name, make } of PAYLOAD_MATRIX) {
    const payload = make()
    bench(name, () => {
      encodeWireValue(payload)
    }, MICRO_OPTS)
  }
})

describe('codec decodeWireValue', () => {
  for (const { name, make } of PAYLOAD_MATRIX) {
    // encodeWireValue output is byte-compatible with the Rust encoder, so
    // decoding it measures the same work as decoding a real Result frame.
    const encoded = new Uint8Array(encodeWireValue(make()))
    bench(name, () => {
      decodeWireValue(encoded)
    }, MICRO_OPTS)
  }
})
