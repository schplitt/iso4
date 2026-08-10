/**
 * Host-side value codec microbenchmarks — `serializeValue` /
 * `deserializeValue` per payload shape, isolated from socket and V8-runtime
 * costs.
 *
 * The Rust legs of the same codec live in
 * `native/v8-runtime/benches/micro.rs` (`v8_value` group).
 */

import { bench, describe } from 'vitest'
import { deserializeValue, serializeValue } from '../src/v8-codec.js'
import { PAYLOAD_MATRIX } from './payloads.js'
import { MICRO_OPTS } from './profile.js'

describe('codec serializeValue', () => {
  for (const { name, make } of PAYLOAD_MATRIX) {
    const payload = make()
    bench(name, () => {
      serializeValue(payload)
    }, MICRO_OPTS)
  }
})

describe('codec deserializeValue', () => {
  for (const { name, make } of PAYLOAD_MATRIX) {
    // serializeValue output is byte-identical to what the Rust serializer
    // produces, so decoding it measures the same work as decoding a real
    // Result frame.
    const encoded = new Uint8Array(serializeValue(make()))
    bench(name, () => {
      deserializeValue(encoded)
    }, MICRO_OPTS)
  }
})
