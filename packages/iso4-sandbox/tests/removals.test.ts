/**
 * Pins for globals the runtime deletes from every context (removals.js).
 * SharedArrayBuffer's pin lives with the timing posture in clock.test.ts.
 */

import type { Sandbox } from '../src/index'
import { afterAll, beforeAll, describe, expect, test } from 'vitest'
import { createSandbox } from '../src/index'

let runtime: Sandbox

beforeAll(async () => {
  runtime = await createSandbox()
})

afterAll(async () => {
  await runtime?.dispose()
})

// Run a snippet and return its default export (fails the test on error).
async function evalDefault(code: string): Promise<unknown> {
  const result = await runtime.run({ code })
  expect(result.ok, result.ok ? undefined : JSON.stringify(result.error)).toBe(true)
  if (!result.ok)
    throw new Error('unreachable')
  return result.exports.default
}

describe('removed globals', () => {
  test('WebAssembly is not exposed (its shared Memory would mint a SharedArrayBuffer)', async () => {
    const ok = await evalDefault(`
      export default typeof WebAssembly === 'undefined'
        && !('WebAssembly' in globalThis)
    `)
    expect(ok).toBe(true)
  })
})
