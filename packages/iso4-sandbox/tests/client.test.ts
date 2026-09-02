import { mkdtemp, rm } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { createServer } from 'node:net'
import type { Server, Socket } from 'node:net'
import { afterEach, describe, expect, test } from 'vitest'
import { RuntimeIpcClient } from '../src/client'
import {
  FrameReader,
  HelloStatus,
  PROTOCOL_VERSION,
  RustToTsMessageTypes,
  TsToRustMessageTypes,
  decodeAuthenticatePayload,
  encodeHelloPayload,
  encodeRustToTsFrame,
} from '../src/ipc'
import { serializationProbe, serializeValue } from '../src/v8-codec'
import { DESCRIPTOR_TOKEN_LEN, StreamSourceRegistry } from '../src/web-codec'
import { Buffer } from 'node:buffer'

let server: Server | undefined
let dir: string | undefined

async function cleanup(): Promise<void> {
  await new Promise<void>((resolve) => {
    if (!server) {
      resolve()
      return
    }
    server.close(() => resolve())
  })
  server = undefined

  if (dir) {
    await rm(dir, { force: true, recursive: true })
    dir = undefined
  }
}

afterEach(async () => {
  await cleanup()
})

async function listen(
  handler: (socket: Socket) => void | Promise<void>,
): Promise<string> {
  dir = await mkdtemp(join(tmpdir(), 'iso4-client-test-'))
  const socketPath = join(dir, 'runtime.sock')
  server = createServer((socket) => {
    handler(socket)
  })

  await new Promise<void>((resolve, reject) => {
    server?.once('error', reject)
    server?.listen(socketPath, () => {
      server?.off('error', reject)
      resolve()
    })
  })

  return socketPath
}

/**
 * Answer the v2 handshake the way the runtime does: an accepting `Hello`
 * carrying this process's own serialization probe (which the client
 * deserializes to prove the format is mutually readable).
 * @param socket
 */
function writeHello(socket: Socket): void {
  socket.write(
    encodeRustToTsFrame(
      RustToTsMessageTypes.Hello,
      encodeHelloPayload({ status: HelloStatus.Ok, probe: serializationProbe(), message: '' }),
    ),
  )
}

/**
 * A `Result` frame payload stub: the `runId` being answered followed by
 * arbitrary body bytes. The runId is not decoration — the client rejects any
 * `Result` whose runId is not the one it sent, so a fake runtime has to echo it
 * the way `session.rs` does.
 * @param runId
 * @param body
 */
function resultPayload(runId: number, body: string): Buffer {
  const head = Buffer.allocUnsafe(4)
  head.writeUInt32BE(runId, 0)
  return Buffer.concat([head, Buffer.from(body, 'utf8')])
}

/**
 * A `BridgeCall` frame payload targeting a global: the run and call ids the
 * router routes by, followed by the export name and the serialized args array.
 * @param runId
 * @param callId
 * @param exportName
 * @param args
 */
function bridgeCallPayload(runId: number, callId: number, exportName: string, args: unknown[] = []): Buffer {
  const name = Buffer.from(exportName, 'utf8')
  const nameLen = Buffer.allocUnsafe(4)
  nameLen.writeUInt32BE(name.byteLength, 0)
  const argsBlob = serializeValue(args)
  const argsLen = Buffer.allocUnsafe(4)
  argsLen.writeUInt32BE(argsBlob.byteLength, 0)
  const head = Buffer.allocUnsafe(10)
  head.writeUInt32BE(runId, 0)
  head.writeUInt32BE(callId, 4)
  head.writeUInt8(0, 8) // targetKind = global
  head.writeUInt8(0, 9) // specifier absent
  return Buffer.concat([head, nameLen, name, argsLen, argsBlob])
}

/**
 * A `Result` stub whose `backgroundPending` flag is set: `ok = 1`, the
 * second-to-last byte carries the flag, the last is `failurePresent = 0` —
 * the exact bytes `peekRunCompletionBackgroundPending` inspects.
 * @param runId
 */
function backgroundResultPayload(runId: number): Buffer {
  const buf = Buffer.alloc(7)
  buf.writeUInt32BE(runId, 0)
  buf.writeUInt8(1, 4) // ok
  buf.writeUInt8(1, 5) // backgroundPending
  buf.writeUInt8(0, 6) // failurePresent
  return buf
}

/**
 * A minimal settled `RunComplete` payload: empty logs, no bridge records,
 * no error — decodable by `decodeRunCompletePayload`.
 * @param runId
 */
function runCompletePayload(runId: number): Buffer {
  const buf = Buffer.alloc(34)
  buf.writeUInt32BE(runId, 0)
  buf.writeUInt8(0, 4) // status = settled
  buf.writeDoubleBE(5, 5) // durationMs
  buf.writeDoubleBE(1, 13) // cpuTimeMs
  // stdout/stderr/bridgeCalls counts (u32 × 3) and errorPresent stay zero
  return buf
}

const descriptorToken = new Uint8Array(DESCRIPTOR_TOKEN_LEN).fill(0xAB)

describe('RuntimeIpcClient', () => {
  test('connects, authenticates, sends Run, and receives Result', async () => {
    const socketPath = await listen(async (socket) => {
      const reader = new FrameReader()
      socket.on('data', (chunk) => reader.push(chunk))

      const authFrame = await reader.readFrame()
      expect(authFrame.messageType).toBe(TsToRustMessageTypes.Authenticate)
      const auth = decodeAuthenticatePayload(authFrame.payload)
      expect(auth.protocolVersion).toBe(PROTOCOL_VERSION)
      // The probe is a serialized `null`; byte 1 is Node's format version.
      expect(Buffer.from(auth.probe)).toEqual(serializationProbe())
      expect(Buffer.from(auth.descriptorToken)).toEqual(Buffer.from(descriptorToken))
      writeHello(socket)

      const runFrame = await reader.readFrame()
      expect(runFrame.messageType).toBe(TsToRustMessageTypes.Run)
      // RunPayload: u32 runId + u32 codeLen + code + ...
      const view = Buffer.from(runFrame.payload.buffer, runFrame.payload.byteOffset, runFrame.payload.byteLength)
      const codeLen = view.readUInt32BE(4)
      const code = view.subarray(8, 8 + codeLen).toString('utf8')
      expect(code).toBe('export default 42')

      // Result frame carries the full RunCompletionPayload (no StdioChunk
      // in the real protocol — logs are inside the Result payload). Its first
      // field is the runId being answered, which the client matches against
      // the one it sent before accepting the frame.
      socket.write(
        encodeRustToTsFrame(
          RustToTsMessageTypes.Result,
          resultPayload(view.readUInt32BE(0), 'payload'),
        ),
      )
    })

    const client = await RuntimeIpcClient.connect({ socketPath, descriptorToken })
    const result = await client.runRawCode('export default 42')

    expect(Buffer.from(result.result).subarray(4).toString('utf8')).toBe('payload')

    await client.dispose()
  })

  test('stats() decodes a StatsResult and rejects unexpected frame types', async () => {
    // StatsPayload: all-zero counters, a 7-byte budget mark, zero RSS,
    // latch off, no prefixes (§5.9 layout).
    const statsPayload = Buffer.alloc(4 * 3 + 8 + 8 + 8 + 1 + 4)
    statsPayload.writeBigUInt64BE(7n, 4 + 4 + 4 + 8)
    const socketPath = await listen(async (socket) => {
      const reader = new FrameReader()
      socket.on('data', (chunk) => reader.push(chunk))
      await reader.readFrame() // Authenticate
      writeHello(socket)

      // First Stats request: answered correctly, with a Log frame in front
      // (legal on any connection, must be skipped).
      await reader.readFrame()
      socket.write(encodeRustToTsFrame(RustToTsMessageTypes.Log, Buffer.from([0, 0, 0, 0, 0])))
      socket.write(encodeRustToTsFrame(RustToTsMessageTypes.StatsResult, statsPayload))

      // Second Stats request: answered with a frame that is illegal on the
      // control connection — the client must fail loudly, not wait forever.
      await reader.readFrame()
      socket.write(encodeRustToTsFrame(RustToTsMessageTypes.Result, Buffer.from('nonsense')))
    })

    const client = await RuntimeIpcClient.connect({ socketPath, descriptorToken })
    const stats = await client.stats()
    expect(stats.warmBudgetBytes).toBe(7)
    expect(stats.prefixes).toEqual([])

    await expect(client.stats()).rejects.toThrow(/Result frame carries runId/)
    await client.dispose()
  })

  test('precompile() rejects unexpected frame types (aligned with stats())', async () => {
    const socketPath = await listen(async (socket) => {
      const reader = new FrameReader()
      socket.on('data', (chunk) => reader.push(chunk))
      await reader.readFrame() // Authenticate
      writeHello(socket)

      // Precompile is bridge-less: a Result frame here is protocol desync.
      await reader.readFrame()
      socket.write(encodeRustToTsFrame(RustToTsMessageTypes.Result, Buffer.from('nonsense')))
    })

    const client = await RuntimeIpcClient.connect({ socketPath, descriptorToken })
    await expect(client.precompile({ code: 'export const x = 1' }))
      .rejects
      .toThrow(/Result frame carries runId/)
    await client.dispose()
  })

  test('rejects when the runtime reports a handshake error status', async () => {
    const socketPath = await listen(async (socket) => {
      const reader = new FrameReader()
      socket.on('data', (chunk) => reader.push(chunk))
      await reader.readFrame() // Authenticate
      socket.write(
        encodeRustToTsFrame(
          RustToTsMessageTypes.Hello,
          encodeHelloPayload({
            status: HelloStatus.V8FormatMismatch,
            probe: serializationProbe(),
            message: 'V8 serialization format mismatch between Node 99 and iso4-v8',
          }),
        ),
      )
    })

    await expect(
      RuntimeIpcClient.connect({ socketPath, descriptorToken }),
    ).rejects.toThrow(/V8 serialization format mismatch/)
  })

  test('rejects when the runtime probe cannot be deserialized here', async () => {
    // A probe claiming a serialization format this Node cannot read. The
    // version byte alone would not catch a corrupt blob, so the client
    // deserializes the probe empirically.
    const socketPath = await listen(async (socket) => {
      const reader = new FrameReader()
      socket.on('data', (chunk) => reader.push(chunk))
      await reader.readFrame() // Authenticate
      socket.write(
        encodeRustToTsFrame(
          RustToTsMessageTypes.Hello,
          encodeHelloPayload({
            status: HelloStatus.Ok,
            probe: Buffer.from([0xFF, 0x63, 0x30]),
            message: '',
          }),
        ),
      )
    })

    await expect(
      RuntimeIpcClient.connect({ socketPath, descriptorToken }),
    ).rejects.toThrow(/V8 serialization format mismatch/)
  })

  test('rejects when the first runtime frame is not a Hello', async () => {
    const socketPath = await listen(async (socket) => {
      const reader = new FrameReader()
      socket.on('data', (chunk) => reader.push(chunk))
      await reader.readFrame() // Authenticate
      socket.write(
        encodeRustToTsFrame(RustToTsMessageTypes.Result, Buffer.from('nope')),
      )
    })

    await expect(
      RuntimeIpcClient.connect({ socketPath, descriptorToken }),
    ).rejects.toThrow(/expected a Hello frame/)
  })

  test('handles BridgeCall: dispatches to handler, sends BridgeResponse, awaits Result', async () => {
    // Simulate Rust sending a BridgeCall mid-run, then sending a Result.
    // Build the BridgeCall payload manually:
    //   u32 runId=1, u32 callId=0, u8 targetKind=0, u8 specifierPresent=0,
    //   String "greet" (u32 len + bytes),
    //   value slot: u32 blobLen + one blob holding the whole args array
    const enc = (s: string) => {
      const b = Buffer.from(s, 'utf8')
      const h = Buffer.allocUnsafe(4)
      h.writeUInt32BE(b.byteLength, 0)
      return Buffer.concat([h, b])
    }
    const argsBlob = serializeValue(['world'])
    const argsLen = Buffer.allocUnsafe(4)
    argsLen.writeUInt32BE(argsBlob.byteLength, 0)
    const bridgeCallPayload = Buffer.concat([
      Buffer.from([0, 0, 0, 1]), // runId = 1 (the client's first run)
      Buffer.from([0, 0, 0, 0]), // callId = 0
      Buffer.from([0]), // targetKind = global
      Buffer.from([0]), // specifier absent
      enc('greet'), // exportName
      argsLen,
      argsBlob,
    ])

    const socketPath = await listen(async (socket) => {
      const reader = new FrameReader()
      socket.on('data', (chunk) => reader.push(chunk))
      await reader.readFrame() // Authenticate
      writeHello(socket)
      const runFrame = await reader.readFrame() // Run
      const runId = Buffer.from(
        runFrame.payload.buffer,
        runFrame.payload.byteOffset,
        runFrame.payload.byteLength,
      ).readUInt32BE(0)

      socket.write(
        encodeRustToTsFrame(RustToTsMessageTypes.BridgeCall, bridgeCallPayload),
      )

      // Read the BridgeResponse the client sends back
      const responseFrame = await reader.readFrame()
      // BridgeResponse is TS→Rust type byte 0x06
      expect(responseFrame.messageType).toBe(TsToRustMessageTypes.BridgeResponse)

      // Send a Result frame to complete the run, echoing the runId it answers.
      socket.write(
        encodeRustToTsFrame(
          RustToTsMessageTypes.Result,
          resultPayload(runId, 'payload'),
        ),
      )
    })

    const dispatched: unknown[] = []
    const client = await RuntimeIpcClient.connect({ socketPath, descriptorToken })
    const raw = await client.runRawCode('export default 1', {
      globals: [{ kind: 'bridge', name: 'greet' }],
      dispatch: {
        greet: (...args: unknown[]) => {
          dispatched.push(args[0])
          return 'hello'
        },
      },
    })
    expect(Buffer.from(raw.result).subarray(4).toString('utf8')).toBe('payload')
    expect(dispatched).toEqual(['world'])
    await client.dispose()
  })

  test('a BridgeCall for a run not in flight is discarded, not dispatched', async () => {
    // The router owns attribution: a BridgeCall is delivered to the run its
    // leading runId names, and one whose run completed while the frame was in
    // flight is dropped — the mirror of the runtime discarding late
    // BridgeResponses. Dispatching it to whichever run happens to be draining
    // would hand one run's call to another run's handler.
    const foreignRunCall = bridgeCallPayload(9, 2, 'greet')

    let strayFrameType: number | undefined
    const socketPath = await listen(async (socket) => {
      const reader = new FrameReader()
      socket.on('data', (chunk) => reader.push(chunk))
      await reader.readFrame() // Authenticate
      writeHello(socket)
      const runFrame = await reader.readFrame()
      const runId = Buffer.from(
        runFrame.payload.buffer,
        runFrame.payload.byteOffset,
        runFrame.payload.byteLength,
      ).readUInt32BE(0)

      socket.write(
        encodeRustToTsFrame(RustToTsMessageTypes.BridgeCall, foreignRunCall),
      )
      socket.write(
        encodeRustToTsFrame(
          RustToTsMessageTypes.Result,
          resultPayload(runId, 'payload'),
        ),
      )
      // Anything arriving after this point would be a response to the
      // discarded call — record it so the test can prove there was none.
      const stray = await reader.readFrame()
      strayFrameType = stray.messageType
    })

    const dispatched: unknown[] = []
    const client = await RuntimeIpcClient.connect({ socketPath, descriptorToken })
    const raw = await client.runRawCode('export default 1', {
      globals: [{ kind: 'bridge', name: 'greet' }],
      dispatch: {
        greet: (...args: unknown[]) => {
          dispatched.push(args[0])
          return 'hello'
        },
      },
    })

    expect(Buffer.from(raw.result).subarray(4).toString('utf8')).toBe('payload')
    await new Promise((r) => {
      setTimeout(r, 20)
    })
    expect(dispatched).toEqual([])
    expect(strayFrameType).toBeUndefined()
    expect(client.usable).toBe(true)
    await client.dispose()
  })

  test('a bridge response too large to encode is answered as an error, not swallowed', async () => {
    // The success arm caught serializeHostValue but not the encode+write that
    // follows, and the trailing .catch discarded it — so no BridgeResponse was
    // ever written and the sandbox's awaited promise never settled. The run then
    // hung to the wall deadline, or forever under `wallTimeMs: 0`.
    const enc = (s: string): Buffer => {
      const b = Buffer.from(s, 'utf8')
      const h = Buffer.allocUnsafe(4)
      h.writeUInt32BE(b.byteLength, 0)
      return Buffer.concat([h, b])
    }
    const argsBlob = serializeValue([])
    const argsLen = Buffer.allocUnsafe(4)
    argsLen.writeUInt32BE(argsBlob.byteLength, 0)
    const bridgeCallPayload = Buffer.concat([
      Buffer.from([0, 0, 0, 1]), // runId = 1 (the client's first run)
      Buffer.from([0, 0, 0, 3]), // callId = 3
      Buffer.from([0]), // targetKind = global
      Buffer.from([0]), // specifier absent
      enc('big'),
      argsLen,
      argsBlob,
    ])

    let responseOk: number | undefined
    let responseCallId: number | undefined
    let responseMessage: string | undefined
    const socketPath = await listen(async (socket) => {
      const reader = new FrameReader()
      socket.on('data', (chunk) => reader.push(chunk))
      await reader.readFrame() // Authenticate
      writeHello(socket)
      const runFrame = await reader.readFrame()
      const runId = Buffer.from(
        runFrame.payload.buffer,
        runFrame.payload.byteOffset,
        runFrame.payload.byteLength,
      ).readUInt32BE(0)

      socket.write(
        encodeRustToTsFrame(RustToTsMessageTypes.BridgeCall, bridgeCallPayload),
      )

      const responseFrame = await reader.readFrame()
      const view = Buffer.from(
        responseFrame.payload.buffer,
        responseFrame.payload.byteOffset,
        responseFrame.payload.byteLength,
      )
      responseCallId = view.readUInt32BE(4) // after the echoed runId
      responseOk = view.readUInt8(8)
      // Failure layout: u32 runId, u32 callId, u8 ok, String code,
      // String name, String message. Walk to the message.
      let at = 9
      for (let i = 0; i < 2; i++) at += 4 + view.readUInt32BE(at)
      responseMessage = view.subarray(at + 4, at + 4 + view.readUInt32BE(at)).toString('utf8')

      socket.write(
        encodeRustToTsFrame(
          RustToTsMessageTypes.Result,
          resultPayload(runId, 'payload'),
        ),
      )
    })

    const client = await RuntimeIpcClient.connect({ socketPath, descriptorToken })
    const raw = await client.runRawCode('export default 1', {
      globals: [{ kind: 'bridge', name: 'big' }],
      dispatch: {
        // Serializes fine, but the framed payload exceeds the 64 MiB ceiling.
        big: () => new Uint8Array(65 * 1024 * 1024),
      },
    })

    expect(responseCallId).toBe(3)
    expect(responseOk).toBe(0)
    // Pin the path: this must be the framing ceiling, not the
    // serializeHostValue catch that already worked before this fix.
    expect(responseMessage).toMatch(/exceeds max frame length/)
    expect(Buffer.from(raw.result).subarray(4).toString('utf8')).toBe('payload')
    expect(client.usable).toBe(true)

    await client.dispose()
  })

  test('runId wraps past 2³¹ as an unsigned value', async () => {
    // `& 0xffffffff` coerces through ToInt32, so the counter used to go
    // negative at 2³¹ and every later run died in writeU32 with
    // ERR_OUT_OF_RANGE before a byte reached the wire. Seeded at the boundary
    // rather than incremented 2³¹ times.
    let sentRunId: number | undefined
    const socketPath = await listen(async (socket) => {
      const reader = new FrameReader()
      socket.on('data', (chunk) => reader.push(chunk))
      await reader.readFrame() // Authenticate
      writeHello(socket)
      const runFrame = await reader.readFrame()
      sentRunId = Buffer.from(
        runFrame.payload.buffer,
        runFrame.payload.byteOffset,
        runFrame.payload.byteLength,
      ).readUInt32BE(0)
      socket.write(
        encodeRustToTsFrame(
          RustToTsMessageTypes.Result,
          resultPayload(sentRunId, 'payload'),
        ),
      )
    })

    const client = await RuntimeIpcClient.connect({ socketPath, descriptorToken })
    ;(client as unknown as { nextRunId: number }).nextRunId = 0x7FFFFFFF

    await expect(client.runRawCode('export default 42')).resolves.toBeDefined()
    expect(sentRunId).toBe(0x80000000)

    await client.dispose()
  })

  test('rejects a Result carrying another run\'s runId and tears the connection down', async () => {
    // The cross-run disclosure: a connection that went back to the pool while
    // the runtime was still mid-run answers the next run with the previous
    // run's Result. Simulated directly by answering with runId + 1 — the frame
    // is otherwise perfectly well-formed, which is exactly why nothing but the
    // runId can catch it.
    const socketPath = await listen(async (socket) => {
      const reader = new FrameReader()
      socket.on('data', (chunk) => reader.push(chunk))
      await reader.readFrame() // Authenticate
      writeHello(socket)
      const runFrame = await reader.readFrame()
      const sentRunId = Buffer.from(
        runFrame.payload.buffer,
        runFrame.payload.byteOffset,
        runFrame.payload.byteLength,
      ).readUInt32BE(0)

      socket.write(
        encodeRustToTsFrame(
          RustToTsMessageTypes.Result,
          resultPayload(sentRunId + 1, 'someone else\'s exports'),
        ),
      )
    })

    const client = await RuntimeIpcClient.connect({ socketPath, descriptorToken })

    await expect(client.runRawCode('export default 42')).rejects.toThrow(
      /Result frame carries runId .* but no run with that id is awaiting a Result/,
    )
    // The pool must not get this connection back: the peer is answering runs
    // that are not ours, and the stream stays offset for every future run.
    expect(client.usable).toBe(false)

    await client.dispose()
  })

  test('answers an undecodable BridgeCall with an error response and keeps the connection', async () => {
    // Guest-controlled bytes the host cannot rebuild (the real trigger is a
    // host type the sandbox accepted and this Node rejects). The callId sits
    // ahead of the args blob, so the call is still answerable — the run must
    // complete normally and the connection must survive, rather than the
    // decode throw escaping and costing a pool slot.
    const enc = (s: string): Buffer => {
      const b = Buffer.from(s, 'utf8')
      const h = Buffer.allocUnsafe(4)
      h.writeUInt32BE(b.byteLength, 0)
      return Buffer.concat([h, b])
    }
    const garbageArgs = Buffer.from([0xFF, 0xFF, 0xFF, 0xFF])
    const argsLen = Buffer.allocUnsafe(4)
    argsLen.writeUInt32BE(garbageArgs.byteLength, 0)
    const undecodableCall = Buffer.concat([
      Buffer.from([0, 0, 0, 1]), // runId = 1 (the client's first run)
      Buffer.from([0, 0, 0, 7]), // callId = 7
      Buffer.from([0]), // targetKind = global
      Buffer.from([0]), // specifier absent
      enc('greet'),
      argsLen,
      garbageArgs, // not a valid V8 serialization blob
    ])

    let responseOk: number | undefined
    let responseCallId: number | undefined
    const socketPath = await listen(async (socket) => {
      const reader = new FrameReader()
      socket.on('data', (chunk) => reader.push(chunk))
      await reader.readFrame() // Authenticate
      writeHello(socket)
      const runFrame = await reader.readFrame()
      const runId = Buffer.from(
        runFrame.payload.buffer,
        runFrame.payload.byteOffset,
        runFrame.payload.byteLength,
      ).readUInt32BE(0)

      socket.write(
        encodeRustToTsFrame(RustToTsMessageTypes.BridgeCall, undecodableCall),
      )

      // The client must still answer, so the runtime is never left parked.
      const responseFrame = await reader.readFrame()
      expect(responseFrame.messageType).toBe(TsToRustMessageTypes.BridgeResponse)
      const view = Buffer.from(
        responseFrame.payload.buffer,
        responseFrame.payload.byteOffset,
        responseFrame.payload.byteLength,
      )
      responseCallId = view.readUInt32BE(4) // after the echoed runId
      responseOk = view.readUInt8(8)

      socket.write(
        encodeRustToTsFrame(
          RustToTsMessageTypes.Result,
          resultPayload(runId, 'payload'),
        ),
      )
    })

    const dispatched: unknown[] = []
    const client = await RuntimeIpcClient.connect({ socketPath, descriptorToken })
    const raw = await client.runRawCode('export default 1', {
      globals: [{ kind: 'bridge', name: 'greet' }],
      dispatch: {
        greet: (...args: unknown[]) => {
          dispatched.push(args[0])
          return 'hello'
        },
      },
    })

    // Answered as a failure, against the right call, and the handler never ran.
    expect(responseCallId).toBe(7)
    expect(responseOk).toBe(0)
    expect(dispatched).toEqual([])
    // The run completed and the connection is still poolable.
    expect(Buffer.from(raw.result).subarray(4).toString('utf8')).toBe('payload')
    expect(client.usable).toBe(true)

    await client.dispose()
  })
})

describe('RuntimeIpcClient run router (multiplexed)', () => {
  // Production admission still puts one run per connection; these drive the
  // router the way the multiplexing activation will, proving the routing
  // structures are already per-run: N concurrent runs over ONE connection,
  // with bridge traffic, aborts, and waitUntil epilogues interleaved.

  /**
   * Read a frame and return its payload as a Buffer view.
   * @param frame
   * @param frame.payload
   */
  function payloadOf(frame: { payload: Uint8Array }): Buffer {
    return Buffer.from(frame.payload.buffer, frame.payload.byteOffset, frame.payload.byteLength)
  }

  test('routes concurrent runs over one connection, Results out of order', async () => {
    const socketPath = await listen(async (socket) => {
      const reader = new FrameReader()
      socket.on('data', (chunk) => reader.push(chunk))
      await reader.readFrame() // Authenticate
      writeHello(socket)

      const runIds: number[] = []
      for (let i = 0; i < 3; i++)
        runIds.push(payloadOf(await reader.readFrame()).readUInt32BE(0))

      // Answer in reverse arrival order — each Result must still land on the
      // run whose id it carries, not on whoever asked first.
      for (const id of [...runIds].reverse()) {
        socket.write(
          encodeRustToTsFrame(RustToTsMessageTypes.Result, resultPayload(id, `run-${id}`)),
        )
      }
    })

    const client = await RuntimeIpcClient.connect({ socketPath, descriptorToken })
    const runs = [
      client.runRawCode('export default 1'),
      client.runRawCode('export default 2'),
      client.runRawCode('export default 3'),
    ]
    const results = await Promise.all(runs)

    // Run ids are allocated 1..3 in call order; each caller got its own body.
    results.forEach((raw, at) => {
      const body = Buffer.from(raw.result)
      expect(body.readUInt32BE(0)).toBe(at + 1)
      expect(body.subarray(4).toString('utf8')).toBe(`run-${at + 1}`)
    })
    expect(client.usable).toBe(true)
    await client.dispose()
  })

  test('interleaves bridge traffic, an abort, and a waitUntil epilogue across runs', async () => {
    let terminatedRunId: number | undefined
    const echoedResponseRunIds: number[] = []

    const socketPath = await listen(async (socket) => {
      const reader = new FrameReader()
      socket.on('data', (chunk) => reader.push(chunk))
      await reader.readFrame() // Authenticate
      writeHello(socket)

      const runIds: number[] = []
      for (let i = 0; i < 3; i++)
        runIds.push(payloadOf(await reader.readFrame()).readUInt32BE(0))
      const [a, b, c] = runIds as [number, number, number]

      // Bridge call to run B while A and C are also in flight.
      socket.write(
        encodeRustToTsFrame(RustToTsMessageTypes.BridgeCall, bridgeCallPayload(b, 1, 'greet', ['from-b'])),
      )
      const responseB = await reader.readFrame()
      expect(responseB.messageType).toBe(TsToRustMessageTypes.BridgeResponse)
      echoedResponseRunIds.push(payloadOf(responseB).readUInt32BE(0))

      // B completes while A and C stay in flight.
      socket.write(
        encodeRustToTsFrame(RustToTsMessageTypes.Result, resultPayload(b, 'b-done')),
      )

      // The host aborts C: a Terminate carrying C's id arrives; answer it
      // with C's own Result, the way the runtime's graceful abandon does.
      const terminate = await reader.readFrame()
      expect(terminate.messageType).toBe(TsToRustMessageTypes.Terminate)
      terminatedRunId = payloadOf(terminate).readUInt32BE(0)
      socket.write(
        encodeRustToTsFrame(RustToTsMessageTypes.Result, resultPayload(c, 'c-aborted')),
      )

      // A's value arrives with waitUntil work pending; grace-time bridge
      // traffic still routes to A, then the RunComplete ends the run.
      socket.write(
        encodeRustToTsFrame(RustToTsMessageTypes.Result, backgroundResultPayload(a)),
      )
      socket.write(
        encodeRustToTsFrame(RustToTsMessageTypes.BridgeCall, bridgeCallPayload(a, 2, 'greet', ['from-grace'])),
      )
      const responseA = await reader.readFrame()
      expect(responseA.messageType).toBe(TsToRustMessageTypes.BridgeResponse)
      echoedResponseRunIds.push(payloadOf(responseA).readUInt32BE(0))
      socket.write(
        encodeRustToTsFrame(RustToTsMessageTypes.RunComplete, runCompletePayload(a)),
      )
    })

    const dispatched: Array<[string, unknown]> = []
    const client = await RuntimeIpcClient.connect({ socketPath, descriptorToken })

    const greet = (owner: string) => (...args: unknown[]) => {
      dispatched.push([owner, args[0]])
      return `${owner}-ok`
    }
    const controller = new AbortController()
    const runA = client.runRawCode('a', {
      globals: [{ kind: 'bridge', name: 'greet' }],
      dispatch: { greet: greet('A') },
    })
    const runB = client.runRawCode('b', {
      globals: [{ kind: 'bridge', name: 'greet' }],
      dispatch: { greet: greet('B') },
    })
    const runC = client.runRawCode('c', { signal: controller.signal })

    // B finishes first — its bridge call was answered by B's dispatcher.
    const rawB = await runB
    expect(Buffer.from(rawB.result).subarray(4).toString('utf8')).toBe('b-done')

    // Abort C: the graceful path resolves it with the runtime's own Result.
    controller.abort(new Error('lost interest'))
    const rawC = await runC
    expect(Buffer.from(rawC.result).subarray(4).toString('utf8')).toBe('c-aborted')

    // A's value arrives early; the epilogue settles after grace-time bridge
    // traffic and the RunComplete frame.
    const rawA = await runA
    expect(rawA.epilogue).toBeDefined()
    const report = await rawA.epilogue
    expect(report?.status).toBe('settled')

    expect(terminatedRunId).toBe(3) // C was the third run registered
    expect(echoedResponseRunIds).toEqual([2, 1]) // B's call, then A's grace call
    expect(dispatched).toEqual([['B', 'from-b'], ['A', 'from-grace']])
    // Nothing here broke the connection: aborts resolved gracefully and the
    // epilogue ended with its RunComplete.
    expect(client.usable).toBe(true)
    await client.dispose()
  })

  test('a Result for an unknown run id fails every run in flight and tears the connection down', async () => {
    const socketPath = await listen(async (socket) => {
      const reader = new FrameReader()
      socket.on('data', (chunk) => reader.push(chunk))
      await reader.readFrame() // Authenticate
      writeHello(socket)
      await reader.readFrame() // Run 1
      await reader.readFrame() // Run 2

      socket.write(
        encodeRustToTsFrame(RustToTsMessageTypes.Result, resultPayload(99, 'nobody asked')),
      )
    })

    const client = await RuntimeIpcClient.connect({ socketPath, descriptorToken })
    const first = client.runRawCode('export default 1')
    const second = client.runRawCode('export default 2')

    // The desync is attribution loss for the whole connection, so BOTH runs
    // fail — not just whichever one a sequential drain would have blamed.
    await expect(first).rejects.toThrow(/Result frame carries runId 99/)
    await expect(second).rejects.toThrow(/Result frame carries runId 99/)
    expect(client.usable).toBe(false)
    await client.dispose()
  })

  test('connection loss mid-grace releases the run\'s stream sources', async () => {
    // A grace-phase run already resolved at its Result, so no caller-side
    // catch releases its stream registry — the router's teardown has to, or
    // an idle source keeps its host ReadableStream locked forever.
    const socketPath = await listen(async (socket) => {
      const reader = new FrameReader()
      socket.on('data', (chunk) => reader.push(chunk))
      await reader.readFrame() // Authenticate
      writeHello(socket)
      const runFrame = await reader.readFrame()
      const runId = payloadOf(runFrame).readUInt32BE(0)

      socket.write(
        encodeRustToTsFrame(RustToTsMessageTypes.Result, backgroundResultPayload(runId)),
      )
      // Let the Result land, then die mid-grace.
      await new Promise((r) => {
        setTimeout(r, 30)
      })
      socket.destroy()
    })

    const cancelled: unknown[] = []
    const streams = new StreamSourceRegistry()
    // A source whose read never settles: the pump parks on it, so the
    // source stays registered into the grace phase.
    streams.register({
      read: () => new Promise(() => {}),
      cancel: async (reason?: unknown) => {
        cancelled.push(reason)
      },
    } as unknown as ReadableStreamDefaultReader<Uint8Array>, [])

    const client = await RuntimeIpcClient.connect({ socketPath, descriptorToken })
    const raw = await client.runRawCode('export default 1', { streams })
    expect(raw.epilogue).toBeDefined()

    // Teardown resolves the epilogue with `undefined` AND releases the
    // registry — the parked reader is cancelled, nothing stays locked.
    await expect(raw.epilogue).resolves.toBeUndefined()
    expect(streams.sources.size).toBe(0)
    expect(cancelled.length).toBe(1)
    expect(client.usable).toBe(false)
    await client.dispose()
  })

  test('a stream frame carrying runId 0 is a desync', async () => {
    // Stream ids are per-run; with several runs multiplexed on one
    // connection an unattributed stream frame could only be routed by a
    // match-by-stream-id guess across runs. The runtime always tags stream
    // frames with a real run id, so 0 means the sides disagree — the same
    // loud teardown as a stray Result, never a silent wrong-run credit.
    const socketPath = await listen(async (socket) => {
      const reader = new FrameReader()
      socket.on('data', (chunk) => reader.push(chunk))
      await reader.readFrame() // Authenticate
      writeHello(socket)
      await reader.readFrame() // Run 1

      const pull = Buffer.alloc(12)
      pull.writeUInt32BE(0, 0) // runId 0 — unattributed
      pull.writeUInt32BE(1, 4) // streamId
      pull.writeUInt32BE(65536, 8) // credit
      socket.write(encodeRustToTsFrame(RustToTsMessageTypes.StreamPull, pull))
    })

    const client = await RuntimeIpcClient.connect({ socketPath, descriptorToken })
    await expect(client.runRawCode('export default 1')).rejects.toThrow(
      /stream frame carries runId 0/,
    )
    expect(client.usable).toBe(false)
    await client.dispose()
  })

  test('a RunComplete for a run without a pending epilogue is a desync', async () => {
    const socketPath = await listen(async (socket) => {
      const reader = new FrameReader()
      socket.on('data', (chunk) => reader.push(chunk))
      await reader.readFrame() // Authenticate
      writeHello(socket)
      await reader.readFrame() // Run 1

      socket.write(
        encodeRustToTsFrame(RustToTsMessageTypes.RunComplete, runCompletePayload(77)),
      )
    })

    const client = await RuntimeIpcClient.connect({ socketPath, descriptorToken })
    await expect(client.runRawCode('export default 1')).rejects.toThrow(
      /RunComplete frame carries runId 77/,
    )
    expect(client.usable).toBe(false)
    await client.dispose()
  })
})
