/**
 * Host-side value codec microbenchmarks — `serializeValue` /
 * `deserializeValue` per payload shape, isolated from socket and V8-runtime
 * costs.
 *
 * The Rust legs of the same codec live in
 * `native/v8-runtime/benches/micro.rs` (`v8_value` group).
 */

import { Buffer } from 'node:buffer'
import { bench, describe } from 'vitest'
import { deserializeValue, serializeValue } from '../src/v8-codec.js'
import { FrameReader, RustToTsMessageTypes, encodeRustToTsFrame } from '../src/ipc.js'
import { PAYLOAD_MATRIX } from './payloads.js'
import { HEAVY_OPTS, MICRO_OPTS } from './profile.js'

describe('codec serializeValue', () => {
  for (const { name, make } of PAYLOAD_MATRIX) {
    const payload = make()
    bench(name, () => {
      serializeValue(payload)
    }, MICRO_OPTS)
  }
})

/**
 * Frame reassembly: feed one Result frame in socket-sized chunks and read it
 * back, which is what every run's result does on the way in.
 *
 * The cost here is dominated by how the reader accumulates chunks, and the
 * shape of that cost is what matters: with a per-chunk `Buffer.concat` the
 * total copying grows with the square of the frame size, so the large sizes
 * blow out while the small ones look fine. Sizes span the range a result can
 * legitimately reach — `maxExportBytes` defaults to 16 MiB — so a regression
 * shows up as the big frames pulling away from the small ones, not as a
 * uniform slowdown.
 */
describe('frame reader reassembly', () => {
  const CHUNK = 16 * 1024
  for (const kib of [64, 1024, 8192]) {
    const frame = encodeRustToTsFrame(
      RustToTsMessageTypes.Result,
      Buffer.alloc(kib * 1024, 0x61),
    )
    bench(`${kib} KiB result`, () => {
      const reader = new FrameReader()
      // Read first, so the frame is handed out by the push that completes it
      // — the path a live connection takes.
      reader.readFrame().catch(() => {})
      for (let offset = 0; offset < frame.byteLength; offset += CHUNK) {
        reader.push(frame.subarray(offset, Math.min(offset + CHUNK, frame.byteLength)))
      }
    }, HEAVY_OPTS)
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
