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

describe('RuntimeIpcClient', () => {
  test('connects, authenticates, sends Run, and receives Result', async () => {
    const socketPath = await listen(async (socket) => {
      const reader = new FrameReader()
      socket.on('data', (chunk) => reader.push(chunk))

      const authFrame = await reader.readFrame()
      expect(authFrame.messageType).toBe(TsToRustMessageTypes.Authenticate)
      const auth = decodeAuthenticatePayload(authFrame.payload)
      expect(auth.protocolVersion).toBe(PROTOCOL_VERSION)
      expect(auth.token).toBe('dev-token')
      // The probe is a serialized `null`; byte 1 is Node's format version.
      expect(Buffer.from(auth.probe)).toEqual(serializationProbe())
      writeHello(socket)

      const runFrame = await reader.readFrame()
      expect(runFrame.messageType).toBe(TsToRustMessageTypes.Run)
      // RunPayload: u32 runId + u32 codeLen + code + ...
      const view = Buffer.from(runFrame.payload.buffer, runFrame.payload.byteOffset, runFrame.payload.byteLength)
      const codeLen = view.readUInt32BE(4)
      const code = view.subarray(8, 8 + codeLen).toString('utf8')
      expect(code).toBe('export default 42')

      // Result frame carries the full RunCompletionPayload (no StdioChunk
      // in the real protocol — logs are inside the Result payload).
      socket.write(
        encodeRustToTsFrame(RustToTsMessageTypes.Result, Buffer.from('payload')),
      )
    })

    const client = await RuntimeIpcClient.connect({
      socketPath,
      token: 'dev-token',
    })
    const result = await client.runRawCode('export default 42')

    expect(Buffer.from(result.result).toString('utf8')).toBe('payload')

    await client.dispose()
  })

  test('stats() decodes a StatsResult and rejects unexpected frame types', async () => {
    // StatsPayload: all-zero counters, live cap 4, no prefixes (§5.7).
    const statsPayload = Buffer.alloc(4 * 4 + 8 + 4)
    statsPayload.writeUInt32BE(4, 4 + 4 + 4 + 8)
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

    const client = await RuntimeIpcClient.connect({ socketPath, token: 'dev-token' })
    const stats = await client.stats()
    expect(stats.maxLiveIsolates).toBe(4)
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

    const client = await RuntimeIpcClient.connect({ socketPath, token: 'dev-token' })
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
      RuntimeIpcClient.connect({ socketPath, token: 'dev-token' }),
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
      RuntimeIpcClient.connect({ socketPath, token: 'dev-token' }),
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
      RuntimeIpcClient.connect({ socketPath, token: 'dev-token' }),
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
      await reader.readFrame() // Run

      socket.write(
        encodeRustToTsFrame(RustToTsMessageTypes.BridgeCall, bridgeCallPayload),
      )

      // Read the BridgeResponse the client sends back
      const responseFrame = await reader.readFrame()
      // BridgeResponse is TS→Rust type byte 0x06
      expect(responseFrame.messageType).toBe(TsToRustMessageTypes.BridgeResponse)

      // Send a Result frame to complete the run
      socket.write(
        encodeRustToTsFrame(RustToTsMessageTypes.Result, Buffer.from('payload')),
      )
    })

    const dispatched: unknown[] = []
    const client = await RuntimeIpcClient.connect({ socketPath, token: 'dev-token' })
    const raw = await client.runRawCode('export default 1', {
      globals: [{ kind: 'bridge', name: 'greet' }],
      dispatch: {
        greet: (...args: unknown[]) => {
          dispatched.push(args[0])
          return 'hello'
        },
      },
    })
    expect(Buffer.from(raw.result).toString('utf8')).toBe('payload')
    expect(dispatched).toEqual(['world'])
    await client.dispose()
  })
})
