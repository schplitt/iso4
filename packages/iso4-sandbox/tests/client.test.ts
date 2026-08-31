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
import { DESCRIPTOR_TOKEN_LEN } from '../src/web-codec'
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

    await expect(client.stats()).rejects.toThrow(/unexpected frame type 0x02/)
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
      .toThrow(/unexpected frame type 0x02/)
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
    //   u32 callId=0, u8 targetKind=0, u8 specifierPresent=0,
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
      responseCallId = view.readUInt32BE(0)
      responseOk = view.readUInt8(4)
      // Failure layout: u32 callId, u8 ok, String code, String name,
      // String message. Walk to the message.
      let at = 5
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
      /Result frame carries runId .* but run \d+ is in flight/,
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
      responseCallId = view.readUInt32BE(0)
      responseOk = view.readUInt8(4)

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
