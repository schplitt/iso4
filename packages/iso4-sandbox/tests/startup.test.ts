/**
 * `createSandbox` startup failures.
 *
 * The child is a separate OS process, and until `createSandbox` returns a
 * `Sandbox` there is nothing for a caller to call `dispose()` on. So every
 * failure between the spawn and the return has to shut the child down itself,
 * or it outlives the call with nobody holding a handle to it.
 *
 * These use a stand-in binary rather than the real runtime: the failures being
 * checked are "the child died at startup" and "the child never completes a
 * handshake", and the real binary does neither on demand.
 */

import { chmod, mkdtemp, readdir, readFile, rm, stat, writeFile } from 'node:fs/promises'
import { existsSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import process from 'node:process'
import { afterAll, beforeAll, describe, expect, test } from 'vitest'
import { createSandbox } from '../src/index.js'

let dir: string

// Write an executable stand-in runtime and return its path.
async function fakeRuntime(name: string, body: string): Promise<string> {
  const path = join(dir, name)
  await writeFile(path, `#!/usr/bin/env node\n${body}\n`, 'utf8')
  await chmod(path, 0o755)
  return path
}

// Reads the `--socket` value the sandbox passed to the stand-in.
const readsSocketArg = `
const argv = process.argv
const socketPath = argv[argv.indexOf('--socket') + 1]
`

beforeAll(async () => {
  dir = await mkdtemp(join(tmpdir(), 'iso4-startup-'))
})

afterAll(async () => {
  await rm(dir, { recursive: true, force: true })
})

describe('createSandbox startup failures', () => {
  test('a child that exits before its socket appears fails fast and names the exit', async () => {
    const binaryPath = await fakeRuntime('exits.mjs', 'process.exit(3)')

    const started = Date.now()
    await expect(createSandbox({ binaryPath, maxIsolates: 2 })).rejects.toThrow(
      /exited before its socket appeared \(exit code 3/,
    )
    // The socket wait is 5 s. Noticing the exit has to beat waiting it out,
    // both because the wait is wasted and because "socket not available"
    // blames the wrong thing.
    expect(Date.now() - started).toBeLessThan(2_000)
  })

  test('a child that never completes a handshake is not left running', async () => {
    // Binds the socket so the wait succeeds, records its pid, then drops every
    // connection it accepts, which fails the handshake without a timeout.
    const pidFile = join(dir, 'silent.pid')
    const binaryPath = await fakeRuntime('silent.mjs', `
      import { createServer } from 'node:net'
      import { writeFileSync } from 'node:fs'
      ${readsSocketArg}
      const server = createServer((socket) => socket.destroy())
      server.listen(socketPath, () => {
        writeFileSync(${JSON.stringify(pidFile)}, String(process.pid))
      })
    `)

    await expect(createSandbox({ binaryPath, maxIsolates: 2 })).rejects.toThrow()

    const pid = Number(await readFile(pidFile, 'utf8'))
    expect(Number.isInteger(pid)).toBe(true)

    // SIGTERM delivery and exit are asynchronous, so poll rather than assume
    // the process is gone the instant the rejection lands.
    let alive = true
    for (let i = 0; i < 50 && alive; i++) {
      try {
        process.kill(pid, 0)
        await new Promise((resolve) => {
          setTimeout(resolve, 20)
        })
      } catch {
        alive = false
      }
    }
    expect(alive).toBe(false)
  })

  test('a failed startup does not leave its socket file behind', async () => {
    // Binds the socket, reports the path it was given, then exits with the
    // file still on disk: the wait succeeds, connecting fails, and the file is
    // left for the failure path to clean up. `createSandbox` generates that
    // path internally, so the stand-in is how the test learns it.
    const pathFile = join(dir, 'socket-path.txt')
    const binaryPath = await fakeRuntime('abandons-socket.mjs', `
      import { createServer } from 'node:net'
      import { writeFileSync } from 'node:fs'
      ${readsSocketArg}
      const server = createServer(() => {})
      server.listen(socketPath, () => {
        writeFileSync(${JSON.stringify(pathFile)}, socketPath)
        process.exit(0)
      })
    `)

    await expect(createSandbox({ binaryPath, maxIsolates: 1 })).rejects.toThrow()

    const socketPath = await readFile(pathFile, 'utf8')
    expect(socketPath).toMatch(/iso4-v8-.*\.sock$/)
    expect(existsSync(socketPath)).toBe(false)
  })
})

describe('socket placement', () => {
  // Uses the real runtime binary: the property under test is where
  // `createSandbox` puts the socket, and the access control on a unix socket
  // is the directory it lives in. Any other local user must be stopped by the
  // kernel at that directory, so it has to be owner-only and per-sandbox.
  test('the socket lives in an owner-only per-sandbox directory that dispose removes', async () => {
    const outer = await mkdtemp(join(tmpdir(), 'iso4-socketdir-'))
    const previousTmpdir = process.env.TMPDIR
    process.env.TMPDIR = outer
    try {
      const sandbox = await createSandbox({ maxIsolates: 1 })
      try {
        const entries = await readdir(outer, { withFileTypes: true })
        expect(entries.length).toBe(1)
        expect(entries.every((entry) => entry.isDirectory())).toBe(true)
        for (const entry of entries) {
          const mode = (await stat(join(outer, entry.name))).mode & 0o777
          expect(mode).toBe(0o700)
        }
      } finally {
        await sandbox.dispose()
      }
      expect(await readdir(outer)).toEqual([])
    } finally {
      if (previousTmpdir === undefined)
        delete process.env.TMPDIR
      else
        process.env.TMPDIR = previousTmpdir
      await rm(outer, { recursive: true, force: true })
    }
  }, 20_000)
})
