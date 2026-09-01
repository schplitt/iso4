import { Buffer } from 'node:buffer'
import { deserializeValue, serializeValue } from './v8-codec.js'
import { DESCRIPTOR_TOKEN_LEN } from './web-codec.js'
import type {
  BridgeCallEntry,
  CallResult,
  ResourceLimits,
  ResetCause,
  RunErrorCode,
  RunResult,
  SandboxExports,
} from './types.js'

/**
 * The limits shape the wire carries. `memoryMb` left `ResourceLimits` when it
 * became a Runtime-level setting (uniform heap cap, set at isolate
 * creation, impossible to renegotiate per run) — but the frame layout is
 * unchanged, so the encoder takes the public limits plus the sandbox-level
 * `memoryMb` injected by the `Sandbox`/`Prefix` implementations.
 */
export type WireResourceLimits = ResourceLimits & { memoryMb?: number }

export const PROTOCOL_VERSION: 1 = 1

export const DEFAULT_MAX_FRAME_LENGTH: number = 64 * 1024 * 1024

export const TsToRustMessageTypes = {
  Authenticate: 0x01,
  Run: 0x02,
  Precompile: 0x03,
  PrefixRun: 0x04,
  DisposePrefix: 0x05,
  BridgeResponse: 0x06,
  Terminate: 0x07,
  /**
   * Request a capacity/usage snapshot. Empty payload; answered with a
   * `StatsResult` frame.
   */
  Stats: 0x08,
  /**
   * One chunk of a streamed host-type body — only inside the granted credit
   * window.
   */
  StreamChunk: 0x09,
  /**
   * End of a streamed body: clean EOF or a source failure.
   */
  StreamEnd: 0x0A,
} as const

export type TsToRustMessageType
  = (typeof TsToRustMessageTypes)[keyof typeof TsToRustMessageTypes]

export const RustToTsMessageTypes = {
  BridgeCall: 0x01,
  Result: 0x02,
  PrecompileResult: 0x03,
  Log: 0x04,
  /**
   * Handshake acknowledgement — the first frame the runtime sends on a new
   * connection, answering `Authenticate`.
   */
  Hello: 0x05,
  /**
   * Capacity/usage snapshot answering a `Stats` request.
   */
  StatsResult: 0x06,
  /**
   * Final frame of a run whose Result reported pending background work:
   * carries the `waitUntil` epilogue outcome and frees the run's
   * connection slot.
   */
  RunComplete: 0x07,
  /**
   * Streaming-body credit grant: the sandbox consumed bytes, the host may
   * have that many more in flight.
   */
  StreamPull: 0x08,
  /**
   * The sandbox cancelled a streamed body; stop pumping, release the source.
   */
  StreamCancel: 0x09,
} as const

export type RustToTsMessageType
  = (typeof RustToTsMessageTypes)[keyof typeof RustToTsMessageTypes]

export interface Frame {
  messageType: number
  payload: Uint8Array
}

export interface TypedFrame<MessageType extends number> {
  messageType: MessageType
  payload: Uint8Array
}

export type TsToRustFrame = TypedFrame<TsToRustMessageType>
export type RustToTsFrame = TypedFrame<RustToTsMessageType>

export interface AuthenticatePayload {
  protocolVersion: number
  /**
   * V8 serialization probe — a serialized `null`, whose second byte is the
   * format version this Node writes. The runtime hard-fails the connection
   * when it cannot read that version (see `HelloPayload`).
   */
  probe: Uint8Array
  /**
   * Random per-sandbox descriptor token, exactly 16 bytes
   * (`docs/protocol.md` §5.1). Host-emitted host-type descriptors are stamped
   * with the brand key derived from it (`web-codec.ts`), and the runtime
   * rehydrates only stamped descriptors. Every connection of one sandbox
   * sends the same token.
   */
  descriptorToken: Uint8Array
}

/**
 * Handshake status the runtime reports on its `Hello` frame.
 */
export const HelloStatus = {
  Ok: 0,
  ProtocolVersionMismatch: 1,
  V8FormatMismatch: 2,
} as const

export type HelloStatusCode = (typeof HelloStatus)[keyof typeof HelloStatus]

export interface HelloPayload {
  status: number
  /**
   * The runtime's own V8 serialization probe (a serialized `null`). The client
   * deserializes it to prove the runtime's format version is readable here.
   */
  probe: Uint8Array
  /**
   * Human-readable detail for a non-zero status; empty when `status = 0`.
   */
  message: string
}

interface PendingRead {
  resolve: (frame: Frame) => void
  reject: (error: Error) => void
}

export class FrameReader {
  /**
   * Chunks exactly as the socket delivered them, oldest first, never merged
   * on arrival.
   *
   * The obvious alternative — one accumulation buffer, each chunk appended
   * with `Buffer.concat` — copies everything received so far on every chunk,
   * so the total copying grows with the square of the frame size rather than
   * linearly with it. A result at the default 16 MiB export cap arrives in
   * roughly 250 socket reads and would cost some 2 GiB of copying to receive,
   * all of it synchronous on the event loop, stalling the whole host and not
   * merely the run that asked for it. The size is chosen by sandbox code, so
   * that is untrusted input deciding how much work the host does.
   *
   * Held as a list instead, each byte is copied at most once, when a complete
   * frame is handed out — and not even then when the frame sits inside a
   * single chunk, which is every small frame.
   */
  private chunks: Buffer[] = []
  /**
   * Total bytes across {@link chunks}; kept in step rather than recomputed.
   */
  private buffered = 0
  private readonly pendingReads: PendingRead[] = []
  private closedError: Error | null = null

  push(chunk: Uint8Array): void {
    if (this.closedError !== null) {
      return
    }

    if (chunk.byteLength > 0) {
      // A view, not a copy. Keeping an empty chunk out of the list lets the
      // readers below assume a non-empty head.
      this.chunks.push(Buffer.from(chunk.buffer, chunk.byteOffset, chunk.byteLength))
      this.buffered += chunk.byteLength
    }
    // `push` is called straight from the socket's 'data' listener, so a throw
    // from the drain below would land on the event loop as an uncaughtException
    // and take the host process down. Route it through `close` instead: pending
    // and future reads reject with it, which is how every other read failure on
    // this connection already surfaces.
    try {
      this.flushPendingReads()
    } catch (error) {
      this.close(error instanceof Error ? error : new Error(String(error)))
    }
  }

  readFrame(): Promise<Frame> {
    const frame = this.tryReadFrame()
    if (frame !== null) {
      return Promise.resolve(frame)
    }

    if (this.closedError !== null) {
      return Promise.reject(this.closedError)
    }

    return new Promise((resolve, reject) => {
      this.pendingReads.push({ resolve, reject })
    })
  }

  async readRustToTsFrame(): Promise<RustToTsFrame> {
    const frame = await this.readFrame()
    return {
      messageType: parseRustToTsMessageType(frame.messageType),
      payload: frame.payload,
    }
  }

  async * [Symbol.asyncIterator](): AsyncGenerator<RustToTsFrame> {
    for (;;) {
      yield await this.readRustToTsFrame()
    }
  }

  close(error: Error = new Error('frame reader closed')): void {
    if (this.closedError !== null) {
      return
    }

    this.closedError = error
    // Deliver whatever whole frames are already buffered, but never let a throw
    // from that drain escape: the bad length prefix that brought us here is
    // still at the head of the buffer, so `tryReadFrame` throws again, and the
    // rejection loop below would be skipped — leaving the pending read forever
    // unsettled (the run hangs, the pool slot leaks). `close` must also stay
    // non-throwing for its callers, which include the socket event handlers.
    try {
      this.flushPendingReads()
    } catch {
      // Nothing to do: every pending read is settled with `error` below.
    }

    while (this.pendingReads.length > 0) {
      this.pendingReads.shift()?.reject(error)
    }
  }

  private flushPendingReads(): void {
    while (this.pendingReads.length > 0) {
      const frame = this.tryReadFrame()
      if (frame === null) {
        break
      }

      this.pendingReads.shift()?.resolve(frame)
    }
  }

  private tryReadFrame(): Frame | null {
    if (this.buffered < 4) {
      return null
    }

    const length = this.peekLength()
    if (length === 0) {
      const error = new Error('frame length cannot be zero')
      this.close(error)
      throw error
    }
    if (length > DEFAULT_MAX_FRAME_LENGTH) {
      const error = new Error(
        `frame length ${length} exceeds max frame length ${DEFAULT_MAX_FRAME_LENGTH}`,
      )
      this.close(error)
      throw error
    }
    if (this.buffered < 4 + length) {
      return null
    }

    return decodeFrame(this.take(4 + length))
  }

  /**
   * The frame's `u32` length prefix, which the socket may have split across
   * chunks — a 4-byte field arriving in two reads is rare but perfectly legal,
   * and reading it out of the head chunk alone would silently misread it.
   */
  private peekLength(): number {
    const first = this.chunks[0]
    if (first !== undefined && first.byteLength >= 4) {
      return first.readUInt32BE(0)
    }
    let value = 0
    let seen = 0
    for (const chunk of this.chunks) {
      for (let i = 0; i < chunk.byteLength && seen < 4; i++, seen++) {
        // Multiplication rather than `<< 8`: a shift works on signed 32-bit
        // values, so a length with the top bit set would come out negative.
        value = value * 256 + chunk[i]!
      }
      if (seen === 4) {
        break
      }
    }
    return value
  }

  /**
   * Remove and return the first `n` buffered bytes. The caller has already
   * established that `n` bytes are there.
   * @param n how many bytes to take, frame length prefix included
   */
  private take(n: number): Buffer {
    const first = this.chunks[0]!
    // Whole frame inside one chunk — the common case, and no copy at all.
    if (first.byteLength === n) {
      this.chunks.shift()
      this.buffered -= n
      return first
    }
    if (first.byteLength > n) {
      this.chunks[0] = first.subarray(n)
      this.buffered -= n
      return first.subarray(0, n)
    }

    // Spans chunks: one allocation the size of the frame, each byte copied
    // once into it.
    const out = Buffer.allocUnsafe(n)
    let offset = 0
    while (offset < n) {
      const chunk = this.chunks[0]!
      const needed = n - offset
      if (chunk.byteLength <= needed) {
        chunk.copy(out, offset)
        offset += chunk.byteLength
        this.chunks.shift()
      } else {
        chunk.copy(out, offset, 0, needed)
        this.chunks[0] = chunk.subarray(needed)
        offset = n
      }
    }
    this.buffered -= n
    return out
  }
}

export function encodeFrame(
  messageType: number,
  payload: Uint8Array,
  maxFrameLength: number = DEFAULT_MAX_FRAME_LENGTH,
): Buffer {
  if (!Number.isInteger(messageType) || messageType < 0 || messageType > 255) {
    throw new Error(`message type must fit in one byte: ${messageType}`)
  }

  const length = payload.byteLength + 1
  if (length > maxFrameLength) {
    throw new Error(
      `frame length ${length} exceeds max frame length ${maxFrameLength}`,
    )
  }

  const frame = Buffer.allocUnsafe(4 + length)
  frame.writeUInt32BE(length, 0)
  frame[4] = messageType
  Buffer.from(payload.buffer, payload.byteOffset, payload.byteLength).copy(
    frame,
    5,
  )
  return frame
}

export function decodeFrame(
  bytes: Uint8Array,
  maxFrameLength: number = DEFAULT_MAX_FRAME_LENGTH,
): Frame {
  if (bytes.byteLength < 4) {
    throw new Error('frame is missing 4-byte length prefix')
  }

  const view = Buffer.from(bytes.buffer, bytes.byteOffset, bytes.byteLength)
  const length = view.readUInt32BE(0)
  if (length === 0) {
    throw new Error('frame length cannot be zero')
  }
  if (length > maxFrameLength) {
    throw new Error(
      `frame length ${length} exceeds max frame length ${maxFrameLength}`,
    )
  }
  if (view.byteLength < 4 + length) {
    throw new Error('frame body is truncated')
  }
  if (view.byteLength > 4 + length) {
    throw new Error('buffer contains trailing bytes after frame')
  }

  const messageType = view[4]
  if (messageType === undefined) {
    throw new Error('frame is missing message type')
  }

  return {
    messageType,
    payload: view.subarray(5),
  }
}

export function encodeTsToRustFrame(
  messageType: TsToRustMessageType,
  payload: Uint8Array,
): Buffer {
  return encodeFrame(messageType, payload)
}

export function encodeRustToTsFrame(
  messageType: RustToTsMessageType,
  payload: Uint8Array,
): Buffer {
  return encodeFrame(messageType, payload)
}

export function decodeTsToRustFrame(bytes: Uint8Array): TsToRustFrame {
  const frame = decodeFrame(bytes)
  return {
    messageType: parseTsToRustMessageType(frame.messageType),
    payload: frame.payload,
  }
}

export function decodeRustToTsFrame(bytes: Uint8Array): RustToTsFrame {
  const frame = decodeFrame(bytes)
  return {
    messageType: parseRustToTsMessageType(frame.messageType),
    payload: frame.payload,
  }
}

/**
 * Encode an `Authenticate` payload per `docs/protocol.md` §5.1:
 * `u16 protocolVersion`, then `u32 probeLength + probe bytes`, then
 * `u32 tokenLength + descriptor token bytes` (exactly 16).
 * @param auth
 */
export function encodeAuthenticatePayload(
  auth: AuthenticatePayload,
): Buffer {
  if (!Number.isInteger(auth.protocolVersion)) {
    throw new Error('protocolVersion must be an integer')
  }
  if (auth.protocolVersion < 0 || auth.protocolVersion > 0xffff) {
    throw new Error(`protocolVersion out of u16 range: ${auth.protocolVersion}`)
  }
  if (auth.descriptorToken.byteLength !== DESCRIPTOR_TOKEN_LEN) {
    throw new Error(
      `descriptor token must be exactly ${DESCRIPTOR_TOKEN_LEN} bytes, got ${auth.descriptorToken.byteLength}`,
    )
  }

  const token = auth.descriptorToken
  const payload = Buffer.allocUnsafe(2 + 4 + auth.probe.byteLength + 4 + token.byteLength)
  payload.writeUInt16BE(auth.protocolVersion, 0)
  payload.writeUInt32BE(auth.probe.byteLength, 2)
  Buffer.from(auth.probe.buffer, auth.probe.byteOffset, auth.probe.byteLength)
    .copy(payload, 6)
  const tokenStart = 6 + auth.probe.byteLength
  payload.writeUInt32BE(token.byteLength, tokenStart)
  Buffer.from(token.buffer, token.byteOffset, token.byteLength)
    .copy(payload, tokenStart + 4)
  return payload
}

export function decodeAuthenticatePayload(
  payload: Uint8Array,
): AuthenticatePayload {
  if (payload.byteLength < 6) {
    throw new Error('payload too short for Authenticate')
  }

  const view = Buffer.from(payload.buffer, payload.byteOffset, payload.byteLength)
  const probeLength = view.readUInt32BE(2)
  if (view.byteLength < 6 + probeLength + 4) {
    throw new Error('Authenticate payload truncated (probe)')
  }
  const tokenStart = 6 + probeLength
  const tokenLength = view.readUInt32BE(tokenStart)
  if (tokenLength !== DESCRIPTOR_TOKEN_LEN) {
    throw new Error(
      `descriptor token must be exactly ${DESCRIPTOR_TOKEN_LEN} bytes, got ${tokenLength}`,
    )
  }
  if (view.byteLength < tokenStart + 4 + tokenLength) {
    throw new Error('Authenticate payload truncated (descriptor token)')
  }
  if (view.byteLength > tokenStart + 4 + tokenLength) {
    throw new Error('Authenticate payload has trailing bytes')
  }
  return {
    protocolVersion: view.readUInt16BE(0),
    probe: view.subarray(6, 6 + probeLength),
    descriptorToken: view.subarray(tokenStart + 4, tokenStart + 4 + tokenLength),
  }
}

/**
 * Encode a `Hello` payload — the runtime's handshake acknowledgement.
 * Exported for tests and for the mock servers in `tests/client.test.ts`; the
 * real encoder lives in the Rust runtime.
 * @param hello
 */
export function encodeHelloPayload(hello: HelloPayload): Buffer {
  const message = Buffer.from(hello.message, 'utf8')
  const payload = Buffer.allocUnsafe(
    1 + 4 + hello.probe.byteLength + 4 + message.byteLength,
  )
  payload.writeUInt8(hello.status, 0)
  payload.writeUInt32BE(hello.probe.byteLength, 1)
  Buffer.from(hello.probe.buffer, hello.probe.byteOffset, hello.probe.byteLength)
    .copy(payload, 5)
  let offset = 5 + hello.probe.byteLength
  payload.writeUInt32BE(message.byteLength, offset)
  offset += 4
  message.copy(payload, offset)
  return payload
}

export function decodeHelloPayload(payload: Uint8Array): HelloPayload {
  const view = Buffer.from(payload.buffer, payload.byteOffset, payload.byteLength)
  if (view.byteLength < 5) {
    throw new Error('payload too short for Hello')
  }
  const status = view.readUInt8(0)
  const probeLength = view.readUInt32BE(1)
  if (view.byteLength < 5 + probeLength + 4) {
    throw new Error('Hello payload truncated (probe)')
  }
  const probe = view.subarray(5, 5 + probeLength)
  const messageLength = view.readUInt32BE(5 + probeLength)
  const messageStart = 5 + probeLength + 4
  if (view.byteLength < messageStart + messageLength) {
    throw new Error('Hello payload truncated (message)')
  }
  return {
    status,
    probe,
    message: view.subarray(messageStart, messageStart + messageLength).toString('utf8'),
  }
}

// ── Payload encoder helpers ───────────────────────────────────────────────
//
// Mirror of the Rust `PayloadReader` primitive encoders in `ipc.rs`.
// All integers are big-endian per docs/protocol.md §3.

// ── GlobalDef (wire form) ─────────────────────────────────────────────────────

/**
 * Wire-shaped host-global declaration for `Run`/`Precompile`/`PrefixRun`
 * payloads. The runtime installs every global kind natively, so the client
 * sends structured data and generates no sandbox source.
 *
 * - `bridge` — a plain host function; Rust installs a bridge stub under `name`.
 * - `string` — a JS expression Rust evaluates as its own script and sets on
 *   `globalThis[name]`.
 * - `data` — a constant carried as a value blob, materialised natively.
 * - `shim` — a bridge handler (installed as a stub under `handlerName`) plus a
 *   shim expression Rust wraps and sets on `globalThis[name]`.
 *
 * The name of the global is always a plain string here — it reaches the sandbox
 * global object through the V8 API (`object.set`), never through interpolation
 * into an identifier position.
 */
export type GlobalDefPayload
  = | { kind: 'bridge', name: string, enumerable: boolean }
    | { kind: 'string', name: string, expr: string, enumerable: boolean }
    | { kind: 'data', name: string, value: unknown, enumerable: boolean }
    | { kind: 'shim', name: string, shim: string, handlerName: string, enumerable: boolean }

/**
 * Wire tag for each `GlobalDefPayload` kind. Mirrors `HostGlobalDef` on the
 * Rust side (`ipc.rs`).
 */
const GLOBAL_DEF_KIND = {
  bridge: 0,
  string: 1,
  data: 2,
  shim: 3,
} as const

class PayloadWriter {
  readonly parts: Buffer[] = []

  writeU8(n: number): this {
    const b = Buffer.allocUnsafe(1)
    b.writeUInt8(n, 0)
    this.parts.push(b)
    return this
  }

  writeU32(n: number): this {
    const b = Buffer.allocUnsafe(4)
    b.writeUInt32BE(n, 0)
    this.parts.push(b)
    return this
  }

  writeString(s: string): this {
    const encoded = Buffer.from(s, 'utf8')
    this.writeU32(encoded.byteLength)
    this.parts.push(encoded)
    return this
  }

  writeBytes(bytes: Uint8Array): this {
    this.parts.push(Buffer.from(bytes.buffer, bytes.byteOffset, bytes.byteLength))
    return this
  }

  /**
   * Write a value slot: `u32 byteLength` + the V8 serialization blob.
   * The single encoding for every value crossing the boundary — there is no
   * tag byte because there is only one codec (`docs/protocol.md` §4).
   * @param value
   */
  writeValueBlob(value: unknown): this {
    return this.writeLengthPrefixedBytes(serializeValue(value))
  }

  /**
   * Write an already-serialized value slot: `u32 byteLength` + blob bytes.
   * @param blob
   */
  writeLengthPrefixedBytes(blob: Uint8Array): this {
    this.writeU32(blob.byteLength)
    return this.writeBytes(blob)
  }

  writeOptionalString(s: string | undefined): this {
    if (s === undefined) {
      this.writeU8(0)
    } else {
      this.writeU8(1)
      this.writeString(s)
    }
    return this
  }

  writeOptionalU32(n: number | undefined): this {
    if (n === undefined) {
      this.writeU8(0)
    } else {
      this.writeU8(1)
      this.writeU32(n)
    }
    return this
  }

  writeGlobalDefs(defs: readonly GlobalDefPayload[]): this {
    // Wire layout per docs/protocol.md §5.2: a length-prefixed list of
    // tagged global definitions. Each entry is `u8 kind, String name,
    // bool enumerable` followed by a kind-specific tail. See
    // `GlobalDefPayload`.
    this.writeU32(defs.length)
    for (const def of defs) {
      this.writeU8(GLOBAL_DEF_KIND[def.kind])
      this.writeString(def.name)
      this.writeU8(def.enumerable ? 1 : 0)
      switch (def.kind) {
        case 'bridge':
          break
        case 'string':
          this.writeString(def.expr)
          break
        case 'data':
          this.writeValueBlob(def.value)
          break
        case 'shim':
          this.writeString(def.shim)
          this.writeString(def.handlerName)
          break
      }
    }
    return this
  }

  writeImports(imports: readonly ImportBindingPayload[]): this {
    // Wire layout per docs/protocol.md §5.2 and the Rust parser in `ipc.rs`:
    //   u32   count
    //   for each: String specifier, u8 kind,
    //     kind 0 (source): String source
    //     kind 1 (host):   u32 exportCount, then per export:
    //                      String name, HostModuleNode
    this.writeU32(imports.length)
    for (const imp of imports) {
      this.writeString(imp.specifier)
      if ('source' in imp) {
        this.writeU8(0)
        this.writeString(imp.source)
      } else {
        this.writeU8(1)
        this.writeU32(imp.module.length)
        for (const [name, node] of imp.module) {
          this.writeString(name)
          this.writeHostModuleNode(node)
        }
      }
    }
    return this
  }

  writeHostModuleNode(node: HostModuleNodePayload): this {
    // u8 tag: 0 = function leaf, 1 = data leaf (value blob), 2 = object.
    switch (node.kind) {
      case 'function':
        this.writeU8(0)
        break
      case 'data':
        this.writeU8(1)
        this.writeValueBlob(node.value)
        break
      case 'object':
        this.writeU8(2)
        this.writeU32(node.entries.length)
        for (const [key, child] of node.entries) {
          this.writeString(key)
          this.writeHostModuleNode(child)
        }
        break
    }
    return this
  }

  writeImportRebinds(rebinds: readonly ImportRebindPayload[]): this {
    // Wire layout per docs/protocol.md §5.2 (PrefixRun only):
    //   u32   count
    //   for each: String specifier, String path
    this.writeU32(rebinds.length)
    for (const rebind of rebinds) {
      this.writeString(rebind.specifier)
      this.writeString(rebind.path)
    }
    return this
  }

  writeOptionalCall(call: CallPayload | undefined): this {
    // Optional<CallSpec>: presence byte, then `String exportPath` + a value
    // slot holding the pre-serialized argument-array blob.
    if (call === undefined) {
      this.writeU8(0)
    } else {
      this.writeU8(1)
      this.writeString(call.exportPath)
      this.writeLengthPrefixedBytes(call.argsBlob)
    }
    return this
  }

  writeResourceLimits(limits: WireResourceLimits): this {
    // Each field is `Optional<u32>`: the client sends only the limits the
    // caller explicitly set. Absent fields are filled in by the runtime, which
    // owns the default safety posture (see `ResourceLimits` jsdoc). An explicit
    // `0` is distinct from absent — it disables that limit entirely.
    this.writeOptionalU32(limits.memoryMb)
    this.writeOptionalU32(limits.cpuTimeMs)
    this.writeOptionalU32(limits.wallTimeMs)
    this.writeOptionalU32(limits.maxExportBytes)
    this.writeOptionalU32(limits.maxStdoutBytes)
    this.writeOptionalU32(limits.maxStderrBytes)
    this.writeOptionalU32(limits.maxBridgeCallBytes)
    this.writeOptionalU32(limits.maxBridgeCalls)
    this.writeOptionalU32(limits.graceMs)
    return this
  }

  toBuffer(): Buffer {
    return Buffer.concat(this.parts)
  }
}

// ── Payload reader ─────────────────────────────────────────────────────────

/**
 * Raised when a frame payload cannot be decoded: truncated data, an invalid
 * presence byte, trailing bytes, or a value blob that does not hold the shape
 * the slot promises. Internal — never surfaces through the public API.
 */
export class PayloadDecodeError extends Error {
  override readonly name = 'PayloadDecodeError'
}

/**
 * Stateful cursor over a frame payload — the mirror of `PayloadReader` in
 * `ipc.rs`. Every read advances the offset; out-of-bounds reads throw
 * immediately so callers never see partial state.
 */
class PayloadReader {
  private readonly view: DataView
  private offset = 0

  constructor(buf: Uint8Array) {
    // Respect the byteOffset of sliced Uint8Arrays.
    this.view = new DataView(buf.buffer, buf.byteOffset, buf.byteLength)
  }

  get remaining(): number {
    return this.view.byteLength - this.offset
  }

  readU8(): number {
    if (this.remaining < 1)
      throw new PayloadDecodeError('unexpected end of payload reading u8')
    return this.view.getUint8(this.offset++)
  }

  readU32(): number {
    if (this.remaining < 4)
      throw new PayloadDecodeError('unexpected end of payload reading u32')
    const n = this.view.getUint32(this.offset, false) // big-endian
    this.offset += 4
    return n
  }

  readF64(): number {
    if (this.remaining < 8)
      throw new PayloadDecodeError('unexpected end of payload reading f64')
    const n = this.view.getFloat64(this.offset, false) // big-endian
    this.offset += 8
    return n
  }

  /**
   * Read a `u64` (8 big-endian bytes) as a JS number — the u64s on the
   * wire are byte counts (heap, budget, RSS), far below 2^53. A value
   * above that is not a real byte count but frame corruption or
   * misalignment; failing loudly beats returning a silently rounded
   * number.
   */
  readU64(): number {
    if (this.remaining < 8)
      throw new PayloadDecodeError('unexpected end of payload reading u64')
    const n = this.view.getBigUint64(this.offset, false) // big-endian
    this.offset += 8
    if (n > BigInt(Number.MAX_SAFE_INTEGER))
      throw new PayloadDecodeError(`u64 exceeds the JS safe-integer range: ${n}`)
    return Number(n)
  }

  /**
   * Read an `Optional<u64>`: a presence byte, then 8 big-endian bytes.
   * Returned as a JS number — the only u64 on the wire is a heap byte count,
   * far below 2^53.
   */
  readOptionalU64(): number | undefined {
    const present = this.readU8()
    if (present === 0)
      return undefined
    return this.readU64()
  }

  readBool(): boolean {
    const b = this.readU8()
    if (b !== 0 && b !== 1) {
      throw new PayloadDecodeError(
        `invalid bool byte: 0x${b.toString(16).padStart(2, '0')}`,
      )
    }
    return b === 1
  }

  readRawBytes(len: number): Uint8Array {
    if (this.remaining < len) {
      throw new PayloadDecodeError(
        `unexpected end of payload: need ${len} bytes, have ${this.remaining}`,
      )
    }
    const slice = new Uint8Array(
      this.view.buffer,
      this.view.byteOffset + this.offset,
      len,
    )
    this.offset += len
    return slice
  }

  readString(): string {
    return Buffer.from(this.readRawBytes(this.readU32())).toString('utf8')
  }

  readStringList(): string[] {
    const count = this.readU32()
    const items: string[] = []
    for (let i = 0; i < count; i++) items.push(this.readString())
    return items
  }

  /**
   * Read a value slot: `u32 byteLength` + V8 serialization blob.
   */
  readValueBlob(): unknown {
    return deserializeValue(this.readRawBytes(this.readU32()))
  }

  /**
   * Read an `Optional<value slot>`: a presence byte, then the slot when set.
   */
  readOptionalValueBlob(): unknown {
    return this.readU8() === 1 ? this.readValueBlob() : undefined
  }

  assertDone(): void {
    if (this.remaining !== 0) {
      throw new PayloadDecodeError(
        `${this.remaining} trailing bytes after expected end of payload`,
      )
    }
  }
}

// ── ResourceLimits ──────────────────────────────────────────────────────────
//
// The wire form is the public `ResourceLimits` from `types.ts` (single source
// of truth — rich jsdoc and `@default` values live there). Every field is
// optional: the client sends only what the caller explicitly set, and the
// runtime fills any absent field from its own defaults. An explicit `0` is
// distinct from absent and disables that limit entirely. `writeResourceLimits`
// encodes each field as `Optional<u32>` accordingly.

// ── ImportBinding (wire form) ───────────────────────────────────────────────

/**
 * One node of a host-module shape tree, mirrored by `HostModuleNode` in the
 * Rust parser (`ipc.rs`). The tree is plain data — function leaves are bare
 * markers (the runtime assigns handle IDs in tree-walk order), data leaves
 * cross as value blobs, and no JS source is ever generated from the tree.
 */
export type HostModuleNodePayload
  = | { kind: 'function' }
    | { kind: 'data', value: unknown }
    | { kind: 'object', entries: readonly (readonly [string, HostModuleNodePayload])[] }

/**
 * Wire-shaped import declaration for `Run`/`Precompile` payloads.
 *
 * A source module carries ESM text verbatim; a host module carries its shape
 * as ordered `(exportName, node)` pairs. The runtime builds host modules
 * natively — the client generates no sandbox source.
 */
export type ImportBindingPayload
  = | { specifier: string, source: string }
    | { specifier: string, module: readonly (readonly [string, HostModuleNodePayload])[] }

/**
 * One host-import function-leaf rebinding for a `PrefixRun` payload. Only the
 * location crosses the wire — the replacement handler stays in the client's
 * dispatch map. The runtime validates it against the shape declared at
 * precompile time (`ERR_UNDECLARED_BINDING` otherwise), unifying enforcement
 * with the Rust-side check for undeclared globals.
 */
export interface ImportRebindPayload {
  specifier: string
  /**
   * Dot-joined function-leaf path inside the host module
   * (e.g. `"someObj.someMethod"`).
   */
  path: string
}

// ── CallPayload ─────────────────────────────────────────────────────────────

/**
 * A host → sandbox function call carried by a `Run`/`PrefixRun` payload.
 *
 * `exportPath` addresses a callable relative to the module's exports
 * (`"named"`, `"default.fetch"`), never `globalThis`. `argsBlob` is **one**
 * pre-serialized V8 blob holding the argument array — the same convention as
 * `BridgeCall` args (identity between arguments preserved). It is produced by
 * `serializeHostValue` in `index.ts`, because draining a `Request` body is
 * async and this encoder is not.
 */
export interface CallPayload {
  exportPath: string
  argsBlob: Uint8Array
}

// ── RunPayload ──────────────────────────────────────────────────────────────

export interface RunPayloadOptions {
  runId: number
  code: string
  filename?: string
  limits?: ResourceLimits
  globals?: readonly GlobalDefPayload[]
  imports?: readonly ImportBindingPayload[]
  /**
   * When present, the run's result is the called function's return value
   * instead of the exports, resolved against the freshly evaluated module.
   */
  call?: CallPayload
}

/**
 * Encode a `RunPayload` per `docs/protocol.md` §5.2.
 * @param options
 */
export function encodeRunPayload(options: RunPayloadOptions): Buffer {
  return new PayloadWriter()
    .writeU32(options.runId)
    .writeString(options.code)
    .writeOptionalString(options.filename)
    .writeResourceLimits(options.limits ?? {})
    .writeGlobalDefs(options.globals ?? [])
    .writeImports(options.imports ?? [])
    .writeOptionalCall(options.call)
    .toBuffer()
}

// ── PrecompilePayload ──────────────────────────────────────────

export interface PrecompilePayloadOptions {
  code: string
  filename?: string
  limits?: ResourceLimits
  globals?: readonly GlobalDefPayload[]
  imports?: readonly ImportBindingPayload[]
}

/**
 * Encode a `PrecompilePayload` per `docs/protocol.md` §5.2.
 * Same as `RunPayload` without `runId`.
 * @param options
 */
export function encodePrecompilePayload(options: PrecompilePayloadOptions): Buffer {
  return new PayloadWriter()
    .writeString(options.code)
    .writeOptionalString(options.filename)
    .writeResourceLimits(options.limits ?? {})
    .writeGlobalDefs(options.globals ?? [])
    .writeImports(options.imports ?? [])
    .toBuffer()
}

// ── PrefixRunPayload ────────────────────────────────────────────

export interface PrefixRunPayloadOptions {
  prefixId: string
  /**
   * Postfix module source. A `PrefixRun` frame carries a postfix *or* a call
   * — exactly one; the encoder enforces this before any bytes are
   * written, mirroring the Rust parser.
   */
  code?: string
  filename?: string
  limits?: ResourceLimits
  globals?: readonly GlobalDefPayload[]
  /**
   * Host-import function-leaf rebindings. The declared module shapes are
   * frozen with the stored prefix on the Rust side; only rebind locations cross.
   */
  importRebinds?: readonly ImportRebindPayload[]
  /**
   * When present, the run calls into the prefix module's exports instead of
   * evaluating a postfix; the result is the function's return value.
   */
  call?: CallPayload
}

/**
 * Encode a `PrefixRunPayload` per `docs/protocol.md` §5.2.
 * `runId` is managed by the client and injected at call time — not part of
 * the public options interface.
 * @param options
 */
export function encodePrefixRunPayload(
  options: PrefixRunPayloadOptions & { runId: number },
): Buffer {
  if ((options.code === undefined) === (options.call === undefined)) {
    throw new Error(
      '[iso4] PrefixRun must carry exactly one of code or call',
    )
  }
  return new PayloadWriter()
    .writeU32(options.runId)
    .writeString(options.prefixId)
    .writeOptionalString(options.code)
    .writeOptionalString(options.filename)
    .writeResourceLimits(options.limits ?? {})
    .writeGlobalDefs(options.globals ?? [])
    .writeImportRebinds(options.importRebinds ?? [])
    .writeOptionalCall(options.call)
    .toBuffer()
}

// ── DisposePrefixPayload ────────────────────────────────────────────────────

/**
 * Encode a `DisposePrefix` payload — just the PrefixId string.
 * @param prefixId
 */
export function encodeDisposePrefixPayload(prefixId: string): Buffer {
  return new PayloadWriter().writeString(prefixId).toBuffer()
}

// ── TerminatePayload ────────────────────────────────────────────────────────

/**
 * Encode a `Terminate` payload — just the RunId (u32) of the run to stop.
 * Sent when an `AbortSignal` fires mid-run so Rust can gracefully abort the
 * run and reply with a real `ERR_ABORTED` Result (see client.ts).
 * @param runId
 */
export function encodeTerminatePayload(runId: number): Buffer {
  return new PayloadWriter().writeU32(runId).toBuffer()
}

// ── BridgeCall decoder (Rust → TS) ───────────────────────────────────────────

export interface BridgeCallInfo {
  /**
   * The owning run's wire id, leading the frame like every other run-scoped
   * frame. 0 only for direct-API runs with no wire identity (never seen by
   * this client). Echoed back on the `BridgeResponse` so the runtime's
   * session demux routes the answer statelessly.
   */
  runId: number
  callId: number
  /**
   * 0 = global, 1 = import (Phase 7)
   */
  targetKind: 0 | 1
  specifier: string | undefined
  exportName: string
  args: unknown[]
}

/**
 * Decode a `BridgeCallPayload` from a `BridgeCall` frame sent by Rust.
 * Per `docs/protocol.md` §5.4.
 *
 * The whole argument list crosses as **one** value blob holding an array —
 * serializing N arguments together is measurably cheaper than N blobs, and it
 * preserves identity between arguments that reference the same object.
 * @param buf
 */
/**
 * Read just the `callId` off a `BridgeCall` frame payload.
 *
 * `callId` is the first field, ahead of the guest-controlled value blob, so it
 * is still recoverable when {@link decodeBridgeCallPayload} throws on the args.
 * That lets the client answer a call it could not decode instead of stranding
 * the peer on a response that never comes.
 *
 * Returns `undefined` only when the payload is too short to carry one.
 * @param buf
 */
export function peekBridgeCallId(buf: Uint8Array): number | undefined {
  // runId leads the payload; callId is the second field.
  if (buf.byteLength < 8)
    return undefined
  return (
    (buf[4]! << 24) | (buf[5]! << 16) | (buf[6]! << 8) | buf[7]!
  ) >>> 0
}

/**
 * Read just the `runId` off a `BridgeCall` frame payload — the leading
 * field, recoverable even when the args blob fails to decode, so the error
 * response can echo the right run for demux routing.
 * @param buf
 */
export function peekBridgeCallRunId(buf: Uint8Array): number | undefined {
  if (buf.byteLength < 4)
    return undefined
  return (
    (buf[0]! << 24) | (buf[1]! << 16) | (buf[2]! << 8) | buf[3]!
  ) >>> 0
}

export function decodeBridgeCallPayload(buf: Uint8Array): BridgeCallInfo {
  const reader = new PayloadReader(buf)

  const runId = reader.readU32()
  const callId = reader.readU32()
  const targetKindRaw = reader.readU8()
  if (targetKindRaw !== 0 && targetKindRaw !== 1)
    throw new Error(`invalid bridge targetKind: ${targetKindRaw}`)
  const targetKind = targetKindRaw as 0 | 1
  const specifierPresent = reader.readU8()
  const specifier = specifierPresent === 1 ? reader.readString() : undefined
  const exportName = reader.readString()
  const decoded = reader.readValueBlob()
  if (!Array.isArray(decoded)) {
    throw new PayloadDecodeError(
      `BridgeCall args must decode to an array, got ${
        decoded === null ? 'null' : typeof decoded
      }`,
    )
  }
  return { runId, callId, targetKind, specifier, exportName, args: decoded }
}

// ── BridgeResponse encoder (TS → Rust) ──────────────────────────────────────

/**
 * Error reported to the sandbox when a host bridge handler throws or rejects.
 * Mirrors `RunErrorPayload` minus `code` (always `ERR_HOST_BRIDGE`) and
 * `stack` — the host stack is never carried across the bridge because it can
 * expose host file paths and infrastructure details to sandbox code.
 */
export interface BridgeErrorPayload {
  name: string
  message: string
  /**
   * Pre-serialized value blob holding the error's own-enumerable properties
   * beyond `name`/`message`/`stack` (reserved keys). Absent when there are
   * none. The Rust side re-attaches these as direct own properties on the
   * Error it rejects the sandbox promise with.
   *
   * These are host-controlled and cross to untrusted sandbox code verbatim —
   * whatever the handler (or a third-party SDK it re-throws) attached to the
   * error. Sanitising is the handler's responsibility: throw a clean Error
   * rather than forwarding one that may carry credentials or request context.
   * The stack is withheld separately because the runtime populates it
   * implicitly, not the host.
   */
  encodedFields?: Uint8Array
}

/**
 * Build a `BridgeErrorPayload` from whatever a host handler threw.
 *
 * Own-enumerable properties beyond `name`/`message`/`stack` travel as
 * `fields` and reappear as direct own properties on the sandbox-side Error;
 * properties V8 refuses to clone (functions, symbols, …) are silently
 * dropped, mirroring the sandbox → host direction (`RunError.fields`).
 * @param err
 */
export function bridgeErrorPayloadFromUnknown(err: unknown): BridgeErrorPayload {
  // Must never throw: it runs inside a rejection handler whose own failure
  // would be swallowed, leaving the sandbox blocked until the wall timeout.
  try {
    if (typeof err !== 'object' || err === null)
      return { name: 'Error', message: String(err) }

    const obj = err as Record<string, unknown>
    const name = typeof obj.name === 'string' && obj.name.length > 0 ? obj.name : 'Error'
    const message = typeof obj.message === 'string' ? obj.message : String(err)

    const fields: Record<string, unknown> = {}
    let hasFields = false
    for (const key of Object.keys(obj)) {
      if (key === 'name' || key === 'message' || key === 'stack')
        continue
      try {
        const value = obj[key] // may invoke a throwing getter
        serializeValue(value)
        fields[key] = value
        hasFields = true
      } catch {
        continue // not serializable — drop it
      }
    }
    if (!hasFields)
      return { name, message }
    return { name, message, encodedFields: serializeValue(fields) }
  } catch {
    return { name: 'Error', message: 'host handler failed' }
  }
}

/**
 * Encode a `BridgeResponsePayload` per `docs/protocol.md` §5.4.
 *
 * When `ok = true`, `encodedValue` holds the pre-serialized value blob.
 * When `ok = false`, `error` describes the handler rejection. The stack slot
 * is always written as absent — host stacks must not leak into the sandbox.
 * @param runId
 * @param callId
 * @param ok
 * @param encodedValue
 * @param error the handler rejection, when `ok = false`
 */
export function encodeBridgeResponsePayload(
  runId: number,
  callId: number,
  ok: boolean,
  encodedValue?: Uint8Array,
  error?: BridgeErrorPayload,
): Buffer {
  const w = new PayloadWriter()
  // Echo the run id off the BridgeCall being answered — the runtime's
  // session demux routes the response by it, statelessly.
  w.writeU32(runId)
  w.writeU32(callId)
  if (ok) {
    w.writeU8(1) // ok = true
    if (encodedValue !== undefined && encodedValue.byteLength > 0) {
      w.writeU8(1) // value present
      w.writeLengthPrefixedBytes(encodedValue)
    } else {
      w.writeU8(0) // value absent → Undefined
    }
  } else {
    w.writeU8(0) // ok = false
    w.writeString('ERR_HOST_BRIDGE')
    w.writeString(error?.name ?? 'Error')
    w.writeString(error?.message ?? 'host handler failed')
    w.writeU8(0) // stack: never carried host → sandbox
    if (error?.encodedFields !== undefined && error.encodedFields.byteLength > 0) {
      w.writeU8(1) // fields present
      w.writeLengthPrefixedBytes(error.encodedFields)
    } else {
      w.writeU8(0) // no fields
    }
  }
  return w.toBuffer()
}

// ── RunCompletionPayload decoder ───────────────────────────────────────────

export interface DecodedRunCompletion {
  /**
   * Run identifier echoed from the `Run` request.
   */
  runId: number
  result: RunResult
  /**
   * True when `waitUntil` background work is still running runtime-side
   *: a `RunComplete` frame follows on this connection. Always false on
   * failures.
   */
  backgroundPending: boolean
}

/**
 * `decodeRunCompletionPayload` result for a run that carried a call:
 * the value blob holds the called function's return value, not an exports
 * object.
 */
export interface DecodedCallCompletion {
  runId: number
  result: CallResult
  /**
   * See {@link DecodedRunCompletion.backgroundPending}.
   */
  backgroundPending: boolean
}

/**
 * Decoded `RunComplete` frame payload — the `waitUntil` epilogue's
 * outcome. Mirrors `encode_run_complete_payload` in `wire.rs`.
 */
export interface DecodedRunComplete {
  runId: number
  status: 'settled' | 'truncated' | 'failed'
  durationMs: number
  cpuTimeMs: number
  stdout: string[]
  stderr: string[]
  bridgeCalls: BridgeCallEntry[]
  error?: { name: string, message: string }
}

/**
 * Decode a `RunComplete` frame payload.
 * @param buf the frame payload
 */
export function decodeRunCompletePayload(buf: Uint8Array): DecodedRunComplete {
  const reader = new PayloadReader(buf)
  const runId = reader.readU32()
  const statusByte = reader.readU8()
  const status = (['settled', 'truncated', 'failed'] as const)[statusByte]
  if (status === undefined) {
    throw new PayloadDecodeError(`unknown RunComplete status byte: ${statusByte}`)
  }
  const durationMs = reader.readF64()
  const cpuTimeMs = reader.readF64()
  const stdout = reader.readStringList()
  const stderr = reader.readStringList()
  const bridgeCalls = readBridgeCallRecords(reader)
  const errorPresent = reader.readU8()
  const error = errorPresent === 1
    ? { name: reader.readString(), message: reader.readString() }
    : undefined
  reader.assertDone()
  const decoded: DecodedRunComplete = { runId, status, durationMs, cpuTimeMs, stdout, stderr, bridgeCalls }
  if (error !== undefined)
    decoded.error = error
  return decoded
}

/**
 * Streaming bodies — wire constants mirroring `ipc.rs`.
 */
export const STREAM_CHUNK_MAX_BYTES: number = 64 * 1024
export const STREAM_CREDIT_WINDOW_BYTES: number = 256 * 1024
/**
 * Bodies that end within this many bytes take the buffered path unchanged;
 * larger ones stream. Host-side policy, not wire contract.
 */
export const STREAM_PROBE_BYTES: number = 64 * 1024

/**
 * Encode a `StreamChunk` payload: `u32 runId, u32 streamId, Bytes data`.
 * @param runId the run the stream belongs to
 * @param streamId the stream
 * @param data at most {@link STREAM_CHUNK_MAX_BYTES} bytes
 */
export function encodeStreamChunkPayload(runId: number, streamId: number, data: Uint8Array): Buffer {
  const out = Buffer.allocUnsafe(12 + data.byteLength)
  out.writeUInt32BE(runId, 0)
  out.writeUInt32BE(streamId, 4)
  out.writeUInt32BE(data.byteLength, 8)
  Buffer.from(data.buffer, data.byteOffset, data.byteLength).copy(out, 12)
  return out
}

/**
 * Encode a `StreamEnd` payload: `u32 runId, u32 streamId, bool ok,
 * Optional<String> error` (only when `ok = false`).
 * @param runId the run
 * @param streamId the stream
 * @param error present = the source failed with this message
 */
export function encodeStreamEndPayload(runId: number, streamId: number, error?: string): Buffer {
  const writer = new PayloadWriter()
  writer.writeU32(runId)
  writer.writeU32(streamId)
  writer.writeU8(error === undefined ? 1 : 0)
  if (error !== undefined) {
    writer.writeU8(1)
    writer.writeString(error)
  }
  return Buffer.concat(writer.parts)
}

/**
 * Decode a `StreamPull` payload: `u32 runId, u32 streamId, u32 credit`.
 * @param payload the frame payload
 */
export function decodeStreamPullPayload(payload: Uint8Array): { runId: number, streamId: number, credit: number } {
  const reader = new PayloadReader(payload)
  const runId = reader.readU32()
  const streamId = reader.readU32()
  const credit = reader.readU32()
  reader.assertDone()
  return { runId, streamId, credit }
}

/**
 * Decode a `StreamCancel` payload: `u32 runId, u32 streamId, String reason`.
 * @param payload the frame payload
 */
export function decodeStreamCancelPayload(payload: Uint8Array): { runId: number, streamId: number, reason: string } {
  const reader = new PayloadReader(payload)
  const runId = reader.readU32()
  const streamId = reader.readU32()
  const reason = reader.readString()
  reader.assertDone()
  return { runId, streamId, reason }
}

/**
 * Read just the `backgroundPending` flag off a `Result` frame payload,
 * without decoding the rest. Both encoders write strict layouts with no
 * trailing bytes, so on a success payload the flag is the second-to-last byte
 * (followed only by the `failurePresent = 0` byte); failures never carry
 * pending work.
 * @param payload the raw Result frame payload
 */
export function peekRunCompletionBackgroundPending(payload: Uint8Array): boolean {
  if (payload.byteLength < 7)
    return false
  const ok = payload[4] === 1
  return ok && payload[payload.byteLength - 2] === 1
}

/**
 * Read just the `runId` off a `Result` frame payload, without decoding the rest.
 *
 * The client correlates every `Result` against the `runId` it sent before it
 * accepts the frame as its own, so this runs on a payload that may belong to a
 * different run entirely — decoding the value blob first would be both wasted
 * work and a chance for another run's bytes to throw. `runId` is the first
 * field of the layout documented on {@link decodeRunCompletionPayload}.
 *
 * Returns `undefined` when the payload is too short to carry one, which is
 * itself a desync rather than a run this client can claim.
 * @param buf
 */
export function peekRunCompletionRunId(buf: Uint8Array): number | undefined {
  if (buf.byteLength < 4)
    return undefined
  return (
    (buf[0]! << 24) | (buf[1]! << 16) | (buf[2]! << 8) | buf[3]!
  ) >>> 0
}

/**
 * Decode a `RunCompletionPayload` from a `Result` frame payload.
 *
 * The value blob is one slot either way: pass `resultKind: 'call'` when the
 * run carried a call, so the blob decodes as the return `value` instead of
 * the exports object — the wire does not tag it; the host knows what it
 * asked for.
 *
 * Wire layout per `docs/protocol.md` §5.6:
 * ```
 * u32   runId
 * u8    ok
 * u8    successPresent   (1 when ok = 1)
 *   ValueBlob  exports (or the call's return value)
 *   List<String>  skippedExports
 *   List<String>  stdout
 *   List<String>  stderr
 *   f64  durationMs
 *   f64  cpuTimeMs
 *   List<BridgeCallRecord>  bridgeCalls
 *   Optional<u64>  heapUsedBytes   (present for prefix runs)
 * u8    failurePresent   (1 when ok = 0)
 *   String  code
 *   String  name
 *   String  message
 *   Optional<String>  stack
 *   Optional<ValueBlob>  fields
 *   List<String>  stdout
 *   List<String>  stderr
 *   f64  durationMs
 *   f64  cpuTimeMs
 *   List<BridgeCallRecord>  bridgeCalls
 *   Optional<u64>  heapUsedBytes   (present for prefix runs)
 * ```
 * @param buf
 */
export function decodeRunCompletionPayload(buf: Uint8Array): DecodedRunCompletion
export function decodeRunCompletionPayload(buf: Uint8Array, resultKind: 'call'): DecodedCallCompletion
export function decodeRunCompletionPayload(
  buf: Uint8Array,
  resultKind?: 'call',
): DecodedRunCompletion | DecodedCallCompletion {
  const reader = new PayloadReader(buf)
  const runId = reader.readU32()
  const ok = reader.readBool()

  if (ok) {
    const successPresent = reader.readU8()
    if (successPresent !== 1) {
      throw new PayloadDecodeError(
        'expected success present byte = 1 when ok = true',
      )
    }
    const valueRaw = reader.readValueBlob()
    const skippedExports = reader.readStringList()
    const stdout = reader.readStringList()
    const stderr = reader.readStringList()
    const durationMs = reader.readF64()
    const cpuTimeMs = reader.readF64()
    const bridgeCalls = readBridgeCallRecords(reader)
    const heapUsedBytes = reader.readOptionalU64()
    const backgroundPending = reader.readBool()
    reader.readU8() // failurePresent = 0; consumed for forward-compat

    reader.assertDone()
    if (resultKind === 'call') {
      return {
        runId,
        backgroundPending,
        result: {
          status: 'completed',
          ok: true,
          value: valueRaw,
          stdout,
          stderr,
          durationMs,
          cpuTimeMs,
          bridgeCalls,
          heapUsedBytes,
        },
      }
    }
    return {
      runId,
      backgroundPending,
      result: {
        status: 'completed',
        ok: true,
        exports: exportsBlobToExports(valueRaw),
        skippedExports,
        stdout,
        stderr,
        durationMs,
        cpuTimeMs,
        bridgeCalls,
        heapUsedBytes,
      },
    }
  }

  reader.readU8() // successPresent = 0
  const failurePresent = reader.readU8()
  if (failurePresent !== 1) {
    throw new PayloadDecodeError(
      'expected failure present byte = 1 when ok = false',
    )
  }
  const code = reader.readString() as RunErrorCode
  const name = reader.readString()
  const message = reader.readString()
  const stackPresent = reader.readU8()
  const stack = stackPresent === 1 ? reader.readString() : undefined
  const fields = reader.readOptionalValueBlob() as Record<string, unknown> | undefined
  const reset = readResetInfo(reader)
  const stdout = reader.readStringList()
  const stderr = reader.readStringList()
  const durationMs = reader.readF64()
  const cpuTimeMs = reader.readF64()
  const bridgeCalls = readBridgeCallRecords(reader)
  const heapUsedBytes = reader.readOptionalU64()

  reader.assertDone()
  return {
    runId,
    backgroundPending: false,
    result: {
      status: 'failed',
      ok: false,
      error: { code, name, message, stack, fields, ...reset },
      stdout,
      stderr,
      durationMs,
      cpuTimeMs,
      bridgeCalls,
      heapUsedBytes,
    },
  }
}

/**
 * The `ERR_INSTANCE_RESET` extras trailing every `RunErrorPayload`: an
 * optional cause class + culprit run id, present only on that code. Returned
 * as a spreadable fragment so plain errors add nothing to the error object.
 * @param reader
 */
function readResetInfo(
  reader: PayloadReader,
): { resetCause: ResetCause, culpritRunId: number } | undefined {
  const present = reader.readU8()
  if (present !== 1)
    return undefined
  const causeByte = reader.readU8()
  const causes: readonly ResetCause[] = ['cpu', 'memory', 'abort', 'internal', 'wall']
  const resetCause = causes[causeByte]
  if (resetCause === undefined)
    throw new PayloadDecodeError(`invalid instance-reset cause byte: ${causeByte}`)
  return { resetCause, culpritRunId: reader.readU32() }
}

/**
 * Decode `List<BridgeCallRecord>` per `docs/protocol.md` §5.6. Names arrive
 * already resolved — the runtime owns the import handle table and the shim
 * naming convention, so no client-side mapping remains.
 * @param reader
 */
function readBridgeCallRecords(reader: PayloadReader): BridgeCallEntry[] {
  const count = reader.readU32()
  const entries: BridgeCallEntry[] = []
  for (let i = 0; i < count; i++) {
    const name = reader.readString()
    const startMs = reader.readF64()
    const durationMs = reader.readF64()
    const argBytes = reader.readU32()
    const responseBytes = reader.readU32()
    const ok = reader.readBool()
    const blocked = reader.readBool()
    entries.push({
      name,
      startMs,
      durationMs,
      argBytes,
      responseBytes,
      ok,
      blocked,
    })
  }
  return entries
}

function exportsBlobToExports(raw: unknown): SandboxExports {
  if (raw === null || typeof raw !== 'object' || Array.isArray(raw)) {
    throw new PayloadDecodeError(
      `exports must decode to an object, got: ${
        Array.isArray(raw) ? 'array' : typeof raw
      }`,
    )
  }
  return raw as SandboxExports
}

// ── PrecompileResultPayload decoder ────────────────────────────────────────

export type PrecompileResult
  = | { ok: true, prefixId: string }
    | { ok: false, error: { code: string, name: string, message: string, stack?: string } }

/**
 * Decode a `PrecompileResultPayload` from a `PrecompileResult` frame.
 *
 * Wire layout per `docs/protocol.md` §5.6:
 * ```
 * u8    ok
 * u8    prefixIdPresent   (1 when ok = true)
 *   String  prefixId
 * u8    errorPresent      (1 when ok = false)
 *   RunErrorPayload  error
 * ```
 * @param buf
 */
export function decodePrecompileResultPayload(buf: Uint8Array): PrecompileResult {
  const reader = new PayloadReader(buf)
  const ok = reader.readBool()

  if (ok) {
    const prefixIdPresent = reader.readU8()
    if (prefixIdPresent !== 1)
      throw new PayloadDecodeError('expected prefixId present byte = 1 when ok = true')
    const prefixId = reader.readString()
    reader.readU8() // errorPresent = 0
    reader.assertDone()
    return { ok: true, prefixId }
  }

  reader.readU8() // prefixIdPresent = 0
  const errorPresent = reader.readU8()
  if (errorPresent !== 1)
    throw new PayloadDecodeError('expected error present byte = 1 when ok = false')
  const code = reader.readString()
  const name = reader.readString()
  const message = reader.readString()
  const stackPresent = reader.readU8()
  const stack = stackPresent === 1 ? reader.readString() : undefined
  reader.readOptionalValueBlob() // consume; precompile errors never carry fields
  readResetInfo(reader) // consume; precompile errors never carry one
  reader.assertDone()
  return { ok: false, error: { code, name, message, stack } }
}

/**
 * The runtime's capacity/usage snapshot as decoded off a `StatsResult`
 * frame. Raw registry numbers; `sandbox.stats()` merges them with
 * host-side pool counters into the public `SandboxStats` shape.
 */
export interface RuntimeStatsPayload {
  /**
   * Running one-off isolates (`sandbox.run()`).
   */
  oneoffRunning: number
  /**
   * Warm instances currently serving a call.
   */
  warmBusy: number
  /**
   * Idle warm instances ready for reuse.
   */
  warmIdle: number
  /**
   * Summed last-call heap of the idle instances, in bytes.
   */
  idleHeapBytes: number
  /**
   * The warm budget in bytes the runtime sheds against;
   * 0 = watermarks disabled.
   */
  warmBudgetBytes: number
  /**
   * The runtime process's resident set size in bytes at snapshot time
   * (0 when unreadable) — the signal the RSS mark acts on.
   */
  rssBytes: number
  /**
   * True while the shedding latch is held: RSS reached the budget and has
   * not yet fallen back to 4/5 of it.
   */
  underPressure: boolean
  /**
   * Per-prefix instance counts, sorted by prefix id.
   */
  prefixes: { prefixId: string, idle: number, busy: number }[]
}

/**
 * Decode a `StatsPayload` from a `StatsResult` frame.
 *
 * Wire layout per `docs/protocol.md` §5.7:
 * ```
 * u32   oneoffRunning
 * u32   warmBusy
 * u32   warmIdle
 * u64   idleHeapBytes
 * u64   warmBudgetBytes
 * u64   rssBytes
 * u8    underPressure
 * u32   prefixCount, then per prefix:
 *   String  prefixId
 *   u32     idle
 *   u32     busy
 * ```
 * @param buf the `StatsResult` frame payload
 */
export function decodeStatsPayload(buf: Uint8Array): RuntimeStatsPayload {
  const reader = new PayloadReader(buf)
  const oneoffRunning = reader.readU32()
  const warmBusy = reader.readU32()
  const warmIdle = reader.readU32()
  const idleHeapBytes = reader.readU64()
  const warmBudgetBytes = reader.readU64()
  const rssBytes = reader.readU64()
  // readBool, not readU8: any byte other than 0/1 here means the frame is
  // misaligned — fail loudly (strict frame handling on the control path).
  const underPressure = reader.readBool()
  const prefixCount = reader.readU32()
  const prefixes: RuntimeStatsPayload['prefixes'] = []
  for (let i = 0; i < prefixCount; i++) {
    const prefixId = reader.readString()
    const idle = reader.readU32()
    const busy = reader.readU32()
    prefixes.push({ prefixId, idle, busy })
  }
  reader.assertDone()
  return { oneoffRunning, warmBusy, warmIdle, idleHeapBytes, warmBudgetBytes, rssBytes, underPressure, prefixes }
}

export function parseTsToRustMessageType(
  byte: number,
): TsToRustMessageType {
  switch (byte) {
    case TsToRustMessageTypes.Authenticate:
    case TsToRustMessageTypes.Run:
    case TsToRustMessageTypes.Precompile:
    case TsToRustMessageTypes.PrefixRun:
    case TsToRustMessageTypes.DisposePrefix:
    case TsToRustMessageTypes.BridgeResponse:
    case TsToRustMessageTypes.Terminate:
    case TsToRustMessageTypes.Stats:
      return byte
    default:
      throw new Error(`unknown TS->Rust message type: ${formatByte(byte)}`)
  }
}

export function parseRustToTsMessageType(
  byte: number,
): RustToTsMessageType {
  switch (byte) {
    case RustToTsMessageTypes.BridgeCall:
    case RustToTsMessageTypes.Result:
    case RustToTsMessageTypes.PrecompileResult:
    case RustToTsMessageTypes.Log:
    case RustToTsMessageTypes.Hello:
    case RustToTsMessageTypes.StatsResult:
    case RustToTsMessageTypes.RunComplete:
    case RustToTsMessageTypes.StreamPull:
    case RustToTsMessageTypes.StreamCancel:
      return byte
    default:
      throw new Error(`unknown Rust->TS message type: ${formatByte(byte)}`)
  }
}

function formatByte(byte: number): string {
  return `0x${byte.toString(16).padStart(2, '0')}`
}
