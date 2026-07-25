import { Buffer } from 'node:buffer'
import { decodeWireValueFromSlice, encodeWireValue } from './wire.js'
import type { ResourceLimits } from './types.js'

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
} as const

export type TsToRustMessageType
  = (typeof TsToRustMessageTypes)[keyof typeof TsToRustMessageTypes]

export const RustToTsMessageTypes = {
  BridgeCall: 0x01,
  Result: 0x02,
  PrecompileResult: 0x03,
  Log: 0x04,
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
  token: string
}

interface PendingRead {
  resolve: (frame: Frame) => void
  reject: (error: Error) => void
}

export class FrameReader {
  private buffer: Buffer = Buffer.alloc(0)
  private readonly pendingReads: PendingRead[] = []
  private closedError: Error | null = null

  push(chunk: Uint8Array): void {
    if (this.closedError !== null) {
      return
    }

    this.buffer = Buffer.concat([
      this.buffer,
      Buffer.from(chunk.buffer, chunk.byteOffset, chunk.byteLength),
    ])
    this.flushPendingReads()
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
    this.flushPendingReads()

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
    if (this.buffer.byteLength < 4) {
      return null
    }

    const length = this.buffer.readUInt32BE(0)
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
    if (this.buffer.byteLength < 4 + length) {
      return null
    }

    const frameBytes = this.buffer.subarray(0, 4 + length)
    this.buffer = this.buffer.subarray(4 + length)
    return decodeFrame(frameBytes)
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

export function encodeAuthenticatePayload(
  auth: AuthenticatePayload,
): Buffer {
  if (!Number.isInteger(auth.protocolVersion)) {
    throw new Error('protocolVersion must be an integer')
  }
  if (auth.protocolVersion < 0 || auth.protocolVersion > 0xffff) {
    throw new Error(`protocolVersion out of u16 range: ${auth.protocolVersion}`)
  }

  const token = Buffer.from(auth.token, 'utf8')
  const payload = Buffer.allocUnsafe(2 + token.byteLength)
  payload.writeUInt16BE(auth.protocolVersion, 0)
  token.copy(payload, 2)
  return payload
}

export function decodeAuthenticatePayload(
  payload: Uint8Array,
): AuthenticatePayload {
  if (payload.byteLength < 2) {
    throw new Error('payload too short for Authenticate')
  }

  const view = Buffer.from(payload.buffer, payload.byteOffset, payload.byteLength)
  return {
    protocolVersion: view.readUInt16BE(0),
    token: view.subarray(2).toString('utf8'),
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
 * - `data` — a constant carried as a `WireValue`, materialised natively.
 * - `shim` — a bridge handler (installed as a stub under `handlerName`) plus a
 *   shim expression Rust wraps and sets on `globalThis[name]`.
 *
 * The name of the global is always a plain string here — it reaches the sandbox
 * global object through the V8 API (`object.set`), never through interpolation
 * into an identifier position.
 */
export type GlobalDefPayload
  = | { kind: 'bridge', name: string }
    | { kind: 'string', name: string, expr: string }
    | { kind: 'data', name: string, value: unknown }
    | { kind: 'shim', name: string, shim: string, handlerName: string }

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
    // tagged global definitions. Each entry is `u8 kind, String name`
    // followed by a kind-specific tail. See `GlobalDefPayload`.
    this.writeU32(defs.length)
    for (const def of defs) {
      this.writeU8(GLOBAL_DEF_KIND[def.kind])
      this.writeString(def.name)
      switch (def.kind) {
        case 'bridge':
          break
        case 'string':
          this.writeString(def.expr)
          break
        case 'data':
          // The constant crosses as a raw WireValue (no length prefix); Rust
          // decodes exactly one value and advances its cursor.
          this.writeBytes(encodeWireValue(def.value))
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
    //   u32                count
    //   for each: String specifier, String source
    this.writeU32(imports.length)
    for (const imp of imports) {
      this.writeString(imp.specifier)
      this.writeString(imp.source)
    }
    return this
  }

  writeResourceLimits(limits: ResourceLimits): this {
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
    return this
  }

  toBuffer(): Buffer {
    return Buffer.concat(this.parts)
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
 * Wire-shaped import declaration for `Run`/`Precompile`/`PrefixRun` payloads.
 *
 * This is the flat, serialisable form. The richer `ImportDefinition` in
 * `types.ts` (with resolver callbacks, host export functions, etc.) is
 * flattened to this shape by the client before encoding.
 */
export interface ImportBindingPayload {
  specifier: string
  source: string
}

// ── RunPayload ──────────────────────────────────────────────────────────────

export interface RunPayloadOptions {
  runId: number
  code: string
  filename?: string
  limits?: ResourceLimits
  globals?: readonly GlobalDefPayload[]
  imports?: readonly ImportBindingPayload[]
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
  code: string
  filename?: string
  limits?: ResourceLimits
  globals?: readonly GlobalDefPayload[]
  imports?: readonly ImportBindingPayload[]
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
  return new PayloadWriter()
    .writeU32(options.runId)
    .writeString(options.prefixId)
    .writeString(options.code)
    .writeOptionalString(options.filename)
    .writeResourceLimits(options.limits ?? {})
    .writeGlobalDefs(options.globals ?? [])
    .writeImports(options.imports ?? [])
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
 * run and reply with a real `ERR_ABORTED` Result (see #36 / client.ts).
 * @param runId
 */
export function encodeTerminatePayload(runId: number): Buffer {
  return new PayloadWriter().writeU32(runId).toBuffer()
}

// ── BridgeCall decoder (Rust → TS) ───────────────────────────────────────────

export interface BridgeCallInfo {
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
 * @param buf
 */
export function decodeBridgeCallPayload(buf: Uint8Array): BridgeCallInfo {
  const view = Buffer.from(buf.buffer, buf.byteOffset, buf.byteLength)
  let offset = 0

  const readU8 = (): number => {
    if (offset >= view.byteLength)
      throw new Error('BridgeCall payload truncated (u8)')
    return view.readUInt8(offset++)
  }
  const readU32 = (): number => {
    if (offset + 4 > view.byteLength)
      throw new Error('BridgeCall payload truncated (u32)')
    const n = view.readUInt32BE(offset)
    offset += 4
    return n
  }
  const readString = (): string => {
    const len = readU32()
    if (offset + len > view.byteLength)
      throw new Error('BridgeCall payload truncated (string)')
    const s = view.subarray(offset, offset + len).toString('utf8')
    offset += len
    return s
  }
  const readWireValue = (): [unknown, number] => {
    const slice = new Uint8Array(view.buffer, view.byteOffset + offset, view.byteLength - offset)
    const [value, consumed] = decodeWireValueFromSlice(slice)
    offset += consumed
    return [value, consumed]
  }

  const callId = readU32()
  const targetKindRaw = readU8()
  if (targetKindRaw !== 0 && targetKindRaw !== 1)
    throw new Error(`invalid bridge targetKind: ${targetKindRaw}`)
  const targetKind = targetKindRaw as 0 | 1
  const specifierPresent = readU8()
  const specifier = specifierPresent === 1 ? readString() : undefined
  const exportName = readString()
  const argCount = readU32()
  const args: unknown[] = []
  for (let i = 0; i < argCount; i++) {
    const [value] = readWireValue()
    args.push(value)
  }
  return { callId, targetKind, specifier, exportName, args }
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
   * Pre-encoded WireValue bytes for the error's own-enumerable properties
   * beyond `name`/`message`/`stack` (reserved keys). Absent when there are
   * none. The Rust side re-attaches these as direct own properties on the
   * Error it rejects the sandbox promise with.
   */
  encodedFields?: Uint8Array
}

/**
 * Build a `BridgeErrorPayload` from whatever a host handler threw.
 *
 * Own-enumerable properties beyond `name`/`message`/`stack` travel as
 * `fields` and reappear as direct own properties on the sandbox-side Error;
 * properties that cannot be wire-encoded (functions, symbols, cycles, …) are
 * silently dropped, mirroring the sandbox → host direction (`RunError.fields`).
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
        encodeWireValue(value)
        fields[key] = value
        hasFields = true
      } catch {
        continue // non-serializable property — drop it
      }
    }
    if (!hasFields)
      return { name, message }
    return { name, message, encodedFields: encodeWireValue(fields) }
  } catch {
    return { name: 'Error', message: 'host handler failed' }
  }
}

/**
 * Encode a `BridgeResponsePayload` per `docs/protocol.md` §5.4.
 *
 * When `ok = true`, `encodedValue` contains pre-encoded WireValue bytes.
 * When `ok = false`, `error` describes the handler rejection. The stack slot
 * is always written as absent — host stacks must not leak into the sandbox.
 * @param callId
 * @param ok
 * @param encodedValue
 * @param error the handler rejection, when `ok = false`
 */
export function encodeBridgeResponsePayload(
  callId: number,
  ok: boolean,
  encodedValue?: Uint8Array,
  error?: BridgeErrorPayload,
): Buffer {
  const w = new PayloadWriter()
  w.writeU32(callId)
  if (ok) {
    w.writeU8(1) // ok = true
    if (encodedValue !== undefined && encodedValue.byteLength > 0) {
      w.writeU8(1) // value present
      w.writeBytes(encodedValue)
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
      w.writeBytes(error.encodedFields)
    } else {
      w.writeU8(0) // no fields
    }
  }
  return w.toBuffer()
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
      return byte
    default:
      throw new Error(`unknown Rust->TS message type: ${formatByte(byte)}`)
  }
}

function formatByte(byte: number): string {
  return `0x${byte.toString(16).padStart(2, '0')}`
}
