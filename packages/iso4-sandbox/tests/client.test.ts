import { mkdtemp, rm } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { createServer } from 'node:net'
import type { Server, Socket } from 'node:net'
import { afterEach, describe, expect, test } from 'vitest'
import { RuntimeIpcClient } from '../src/client'
import {
  FrameReader,
  PROTOCOL_VERSION,
  RustToTsMessageTypes,
  TsToRustMessageTypes,
  decodeAuthenticatePayload,
  encodeRustToTsFrame,
} from '../src/ipc'
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

describe('RuntimeIpcClient', () => {
  test('connects, authenticates, sends Run, and receives Result', async () => {
    const socketPath = await listen(async (socket) => {
      const reader = new FrameReader()
      socket.on('data', (chunk) => reader.push(chunk))

      const authFrame = await reader.readFrame()
      expect(authFrame.messageType).toBe(TsToRustMessageTypes.Authenticate)
      expect(decodeAuthenticatePayload(authFrame.payload)).toEqual({
        protocolVersion: PROTOCOL_VERSION,
        token: 'dev-token',
      })

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

  test('handles BridgeCall: dispatches to handler, sends BridgeResponse, awaits Result', async () => {
    // Simulate Rust sending a BridgeCall mid-run, then sending a Result.
    // Build the BridgeCall payload manually:
    //   u32 callId=0, u8 targetKind=0, u8 specifierPresent=0,
    //   String "greet" (u32 len + bytes), u32 argCount=1,
    //   WireValue::String "world" (0x05 + u32 len + bytes)
    const enc = (s: string) => {
      const b = Buffer.from(s, 'utf8')
      const h = Buffer.allocUnsafe(4)
      h.writeUInt32BE(b.byteLength, 0)
      return Buffer.concat([h, b])
    }
    const bridgeCallPayload = Buffer.concat([
      Buffer.from([0, 0, 0, 0]), // callId = 0
      Buffer.from([0]), // targetKind = global
      Buffer.from([0]), // specifier absent
      enc('greet'), // exportName
      Buffer.from([0, 0, 0, 1]), // argCount = 1
      Buffer.from([0x05]), // WireValue::String tag
      enc('world'), // string value
    ])

    const socketPath = await listen(async (socket) => {
      const reader = new FrameReader()
      socket.on('data', (chunk) => reader.push(chunk))
      await reader.readFrame() // Authenticate
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
