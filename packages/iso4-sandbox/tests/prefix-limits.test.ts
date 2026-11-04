/**
 * `prepare({ limits })` is the default `ResourceLimits` for every run on that
 * prefix; a run's own `limits` overrides it field by field.
 */

import { afterAll, afterEach, beforeAll, describe, expect, test, vi } from 'vitest'
import { RuntimeIpcClient } from '../src/client.js'
import { createSandbox } from '../src/index.js'
import type { Sandbox } from '../src/types.js'

let sandbox: Sandbox

beforeAll(async () => {
  sandbox = await createSandbox()
})

afterAll(async () => {
  await sandbox?.dispose()
})

afterEach(() => {
  vi.restoreAllMocks()
})

/**
 * The `limits` field of the last PrefixRun frame the client sent.
 * @param spy
 */
function lastWireLimits(spy: ReturnType<typeof vi.spyOn>): Record<string, unknown> {
  const last = spy.mock.calls.at(-1)
  expect(last).toBeDefined()
  return (last![0] as { limits: Record<string, unknown> }).limits
}

describe('prepare({ limits }) as per-run defaults', () => {
  test('a run without limits sends the prefix defaults', async () => {
    const prefix = await sandbox.prepare({
      code: 'export default () => 1',
      limits: { cpuTimeMs: 250, maxBridgeCalls: 3 },
    })
    const spy = vi.spyOn(RuntimeIpcClient.prototype, 'prefixRun')
    await prefix.call({ export: 'default' })
    expect(lastWireLimits(spy)).toMatchObject({ cpuTimeMs: 250, maxBridgeCalls: 3 })
    await prefix.dispose()
  })

  test('per-run limits override field by field; an explicit undefined keeps the default', async () => {
    const prefix = await sandbox.prepare({
      code: 'export default () => 1',
      limits: { cpuTimeMs: 250, maxBridgeCalls: 3 },
    })
    const spy = vi.spyOn(RuntimeIpcClient.prototype, 'prefixRun')
    await prefix.call({ export: 'default', limits: { cpuTimeMs: 500, wallTimeMs: 1_000 } })
    const sent = lastWireLimits(spy)
    expect(sent).toMatchObject({ cpuTimeMs: 500, maxBridgeCalls: 3, wallTimeMs: 1_000 })
    await prefix.call({ export: 'default', limits: { cpuTimeMs: undefined } as never })
    expect(lastWireLimits(spy)).toMatchObject({ cpuTimeMs: 250, maxBridgeCalls: 3 })
    await prefix.dispose()
  })

  test('a field set nowhere is absent on the wire so the runtime default applies', async () => {
    const prefix = await sandbox.prepare({ code: 'export default () => 1', limits: { cpuTimeMs: 250 } })
    const spy = vi.spyOn(RuntimeIpcClient.prototype, 'prefixRun')
    await prefix.call({ export: 'default' })
    expect(lastWireLimits(spy)).not.toHaveProperty('wallTimeMs')
    await prefix.dispose()
  })

  test('the default is enforced by the runtime and a run can loosen it', async () => {
    const prefix = await sandbox.prepare({
      code: 'export default async () => { await tool(); await tool(); return "ok" }',
      globals: { tool: async () => 1 },
      limits: { maxBridgeCalls: 1 },
    })
    const limited = await prefix.call({ export: 'default' })
    expect(limited.ok).toBe(false)
    if (!limited.ok)
      expect(limited.error.code).toBe('ERR_BRIDGE_CALL_LIMIT_EXCEEDED')
    const loosened = await prefix.call({ export: 'default', limits: { maxBridgeCalls: 0 } })
    expect(loosened.status).toBe('completed')
    await prefix.dispose()
  })

  test('an invalid default rejects at prepare() without a round trip', async () => {
    const spy = vi.spyOn(RuntimeIpcClient.prototype, 'precompile')
    await expect(sandbox.prepare({ code: 'export default 1', limits: { cpuTimeMs: -1 } }))
      .rejects
      .toThrow(TypeError)
    await expect(sandbox.prepare({ code: 'export default 1', limits: 5 as never }))
      .rejects
      .toThrow(TypeError)
    expect(spy).not.toHaveBeenCalled()
  })
})

test('the defaults are a snapshot: editing the caller object after prepare() changes nothing', async () => {
  const limits = { cpuTimeMs: 250 }
  const prefix = await sandbox.prepare({ code: 'export default () => 1', limits })
  limits.cpuTimeMs = -1
  const spy = vi.spyOn(RuntimeIpcClient.prototype, 'prefixRun')
  await prefix.call({ export: 'default' })
  expect(lastWireLimits(spy)).toMatchObject({ cpuTimeMs: 250 })
  await prefix.dispose()
})
