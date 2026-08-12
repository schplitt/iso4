/**
 * Frame-payload decoders on the Rust → TS direction:
 * `decodeRunCompletionPayload`, `decodePrecompileResultPayload`, and
 * `decodeBridgeCallPayload` (`docs/protocol.md` §5.4 / §5.6).
 *
 * Every value slot is `u32 byteLength` + a V8 serialization blob, so the
 * fixtures below build known-good bytes with `serializeValue` — the same
 * encoder the Rust side's `blob::serialize_value` is byte-compatible with.
 *
 * The value codec itself is covered by `v8-codec.test.ts`.
 */

import { Buffer } from 'node:buffer'
import { describe, expect, test } from 'vitest'
import {
  PayloadDecodeError,
  decodeBridgeCallPayload,
  decodePrecompileResultPayload,
  decodeRunCompletionPayload,
} from '../src/ipc.js'
import { serializeValue } from '../src/v8-codec.js'
import type { RunErrorCode } from '../src/types.js'

// ── Test-only payload encoders (mirror of the Rust encoders in wire.rs) ─────

function u32(n: number): Buffer {
  const b = Buffer.allocUnsafe(4)
  b.writeUInt32BE(n, 0)
  return b
}

function optionalU64(n: number | undefined): Buffer {
  if (n === undefined)
    return Buffer.from([0])
  const b = Buffer.alloc(9)
  b[0] = 1
  b.writeBigUInt64BE(BigInt(n), 1)
  return b
}

function f64(n: number): Buffer {
  const b = Buffer.allocUnsafe(8)
  b.writeDoubleBE(n, 0)
  return b
}

function str(s: string): Buffer {
  const bytes = Buffer.from(s, 'utf8')
  return Buffer.concat([u32(bytes.byteLength), bytes])
}

function strList(items: readonly string[]): Buffer {
  return Buffer.concat([u32(items.length), ...items.map(str)])
}

/**
 * A value slot: `u32 byteLength` + blob.
 * @param value
 */
function valueSlot(value: unknown): Buffer {
  const blob = serializeValue(value)
  return Buffer.concat([u32(blob.byteLength), blob])
}

/**
 * An `Optional<value slot>`: presence byte, then the slot when set.
 * @param value
 * @param present
 */
function optionalValueSlot(value: unknown, present: boolean): Buffer {
  return present
    ? Buffer.concat([Buffer.from([1]), valueSlot(value)])
    : Buffer.from([0])
}

function optionalString(s: string | undefined): Buffer {
  return s === undefined ? Buffer.from([0]) : Buffer.concat([Buffer.from([1]), str(s)])
}

interface TestBridgeRecord {
  name: string
  startMs: number
  durationMs: number
  argBytes: number
  responseBytes: number
  ok: boolean
  blocked: boolean
}

function bridgeRecords(records: readonly TestBridgeRecord[]): Buffer {
  return Buffer.concat([
    u32(records.length),
    ...records.map((r) => Buffer.concat([
      str(r.name),
      f64(r.startMs),
      f64(r.durationMs),
      u32(r.argBytes),
      u32(r.responseBytes),
      Buffer.from([r.ok ? 1 : 0]),
      Buffer.from([r.blocked ? 1 : 0]),
    ])),
  ])
}

interface SuccessSpec {
  ok: true
  exports: unknown
  skippedExports?: readonly string[]
  stdout?: readonly string[]
  stderr?: readonly string[]
  durationMs?: number
  cpuTimeMs?: number
  bridgeCalls?: readonly TestBridgeRecord[]
  heapUsedBytes?: number
}

interface FailureSpec {
  ok: false
  code: string
  name: string
  message: string
  stack?: string
  fields?: Record<string, unknown>
  stdout?: readonly string[]
  stderr?: readonly string[]
  durationMs?: number
  cpuTimeMs?: number
  bridgeCalls?: readonly TestBridgeRecord[]
  heapUsedBytes?: number
}

function encodeCompletionPayload(runId: number, spec: SuccessSpec | FailureSpec): Buffer {
  const tail = Buffer.concat([
    strList(spec.stdout ?? []),
    strList(spec.stderr ?? []),
    f64(spec.durationMs ?? 0),
    f64(spec.cpuTimeMs ?? 0),
    bridgeRecords(spec.bridgeCalls ?? []),
    optionalU64(spec.heapUsedBytes), // #64: Optional<u64> heapUsedBytes
  ])

  if (spec.ok) {
    return Buffer.concat([
      u32(runId),
      Buffer.from([1, 1]), // ok = true, successPresent = 1
      valueSlot(spec.exports),
      strList(spec.skippedExports ?? []),
      tail,
      Buffer.from([0]), // failurePresent = 0
    ])
  }

  return Buffer.concat([
    u32(runId),
    Buffer.from([0, 0, 1]), // ok = false, successPresent = 0, failurePresent = 1
    str(spec.code),
    str(spec.name),
    str(spec.message),
    optionalString(spec.stack),
    optionalValueSlot(spec.fields, spec.fields !== undefined),
    tail,
  ])
}

function encodeBridgeCallPayload(
  callId: number,
  exportName: string,
  args: readonly unknown[],
  specifier?: string,
): Buffer {
  return Buffer.concat([
    u32(callId),
    Buffer.from([specifier === undefined ? 0 : 1]), // targetKind
    optionalString(specifier),
    str(exportName),
    valueSlot(args),
  ])
}

// ── decodeRunCompletionPayload — success ───────────────────────────────────

describe('decodeRunCompletionPayload — success', () => {
  test('basic success with default export', () => {
    const { runId, result } = decodeRunCompletionPayload(
      encodeCompletionPayload(0, { ok: true, exports: { default: 42 }, durationMs: 1.5 }),
    )
    expect(runId).toBe(0)
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports['default']).toBe(42)
    expect(result.stdout).toEqual([])
    expect(result.stderr).toEqual([])
    expect(result.durationMs).toBeCloseTo(1.5)
    expect(result.cpuTimeMs).toBe(0)
    expect(result.bridgeCalls).toEqual([])
  })

  test('default plus named exports arrive as one flat object', () => {
    const { result } = decodeRunCompletionPayload(
      encodeCompletionPayload(0, {
        ok: true,
        exports: { default: { ok: true }, count: 2, label: 'x' },
      }),
    )
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports).toEqual({ default: { ok: true }, count: 2, label: 'x' })
  })

  test('an empty module decodes to an empty exports object', () => {
    const { result } = decodeRunCompletionPayload(
      encodeCompletionPayload(0, { ok: true, exports: {} }),
    )
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports).toEqual({})
  })

  test('real instances survive the exports slot', () => {
    const { result } = decodeRunCompletionPayload(
      encodeCompletionPayload(0, {
        ok: true,
        exports: {
          when: new Date(1700000000000),
          bytes: new Uint8Array([1, 2]),
          big: 2n ** 70n,
        },
      }),
    )
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.exports['when']).toBeInstanceOf(Date)
    expect(result.exports['bytes']).toBeInstanceOf(Uint8Array)
    expect(result.exports['big']).toBe(2n ** 70n)
  })

  test('stdout and stderr lines are captured in order', () => {
    const { result } = decodeRunCompletionPayload(
      encodeCompletionPayload(0, {
        ok: true,
        exports: {},
        stdout: ['one', 'two'],
        stderr: ['warn'],
      }),
    )
    expect(result.stdout).toEqual(['one', 'two'])
    expect(result.stderr).toEqual(['warn'])
  })

  test('skipped export names decode alongside the exports', () => {
    const { result } = decodeRunCompletionPayload(
      encodeCompletionPayload(0, {
        ok: true,
        exports: { limits: { memoryMb: 64 } },
        skippedExports: ['default', 'helper'],
      }),
    )
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.skippedExports).toEqual(['default', 'helper'])
    expect(result.exports).toEqual({ limits: { memoryMb: 64 } })
  })

  test('no skipped exports decodes to an empty list', () => {
    const { result } = decodeRunCompletionPayload(
      encodeCompletionPayload(0, { ok: true, exports: {} }),
    )
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.skippedExports).toEqual([])
  })

  test('call mode decodes the value slot as the return value, not exports', () => {
    // The wire slot is identical — the host asked for a call, so the blob is
    // the function's return value, of any shape (here: not an object).
    const { result } = decodeRunCompletionPayload(
      encodeCompletionPayload(0, { ok: true, exports: 'hello from fetch' }),
      'call',
    )
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.value).toBe('hello from fetch')
    expect('exports' in result).toBe(false)
  })

  test('call mode failures decode identically to run failures', () => {
    const { result } = decodeRunCompletionPayload(
      encodeCompletionPayload(0, {
        ok: false,
        code: 'ERR_CALL_TARGET_NOT_FOUND',
        name: 'Error',
        message: 'call export path "default.missing" does not resolve',
      }),
      'call',
    )
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.code).toBe('ERR_CALL_TARGET_NOT_FOUND')
  })

  test('runId is echoed and durationMs preserved exactly', () => {
    const { runId, result } = decodeRunCompletionPayload(
      encodeCompletionPayload(4242, { ok: true, exports: {}, durationMs: 123.456 }),
    )
    expect(runId).toBe(4242)
    expect(result.durationMs).toBe(123.456)
  })

  test('cpuTimeMs and bridge call records decode on success and failure', () => {
    // Names arrive already resolved by the runtime — import calls carry their
    // `<specifier>.<path>` form, shims their public name. argBytes and
    // responseBytes are blob byte counts.
    const records: TestBridgeRecord[] = [
      {
        name: 'fetch',
        startMs: 0.5,
        durationMs: 2.25,
        argBytes: 180,
        responseBytes: 4096,
        ok: true,
        blocked: false,
      },
      {
        name: 'tools:search.query',
        startMs: 3,
        durationMs: 0.5,
        argBytes: 64,
        responseBytes: 0,
        ok: false,
        blocked: false,
      },
    ]
    const success = decodeRunCompletionPayload(
      encodeCompletionPayload(0, {
        ok: true,
        exports: {},
        durationMs: 5,
        cpuTimeMs: 1.25,
        bridgeCalls: records,
      }),
    )
    expect(success.result.cpuTimeMs).toBeCloseTo(1.25)
    expect(success.result.bridgeCalls).toEqual(records)

    const failure = decodeRunCompletionPayload(
      encodeCompletionPayload(0, {
        ok: false,
        code: 'ERR_BRIDGE_CALL_LIMIT_EXCEEDED',
        name: 'Error',
        message: 'limit',
        durationMs: 1,
        cpuTimeMs: 0.75,
        bridgeCalls: [{
          name: 'tool',
          startMs: 0.1,
          durationMs: 0,
          argBytes: 0,
          responseBytes: 0,
          ok: false,
          blocked: true,
        }],
      }),
    )
    expect(failure.result.cpuTimeMs).toBeCloseTo(0.75)
    expect(failure.result.bridgeCalls).toHaveLength(1)
    expect(failure.result.bridgeCalls[0]?.blocked).toBe(true)
  })
})

// ── decodeRunCompletionPayload — failure ───────────────────────────────────

describe('decodeRunCompletionPayload — failure', () => {
  test('compile error with no stack', () => {
    const { result } = decodeRunCompletionPayload(
      encodeCompletionPayload(0, {
        ok: false,
        code: 'ERR_COMPILE',
        name: 'SyntaxError',
        message: 'bad syntax',
      }),
    )
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error).toEqual({
      code: 'ERR_COMPILE',
      name: 'SyntaxError',
      message: 'bad syntax',
      stack: undefined,
      fields: undefined,
    })
  })

  test('runtime error with a stack trace', () => {
    const { result } = decodeRunCompletionPayload(
      encodeCompletionPayload(0, {
        ok: false,
        code: 'ERR_USER_CODE',
        name: 'TypeError',
        message: 'boom',
        stack: 'TypeError: boom\n    at <iso4>:1:1',
      }),
    )
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.stack).toContain('at <iso4>:1:1')
  })

  test('logs produced before the throw are preserved', () => {
    const { result } = decodeRunCompletionPayload(
      encodeCompletionPayload(0, {
        ok: false,
        code: 'ERR_USER_CODE',
        name: 'Error',
        message: 'x',
        stdout: ['before'],
        stderr: ['warned'],
      }),
    )
    expect(result.stdout).toEqual(['before'])
    expect(result.stderr).toEqual(['warned'])
  })

  test('error fields round-trip as a structured object', () => {
    const { result } = decodeRunCompletionPayload(
      encodeCompletionPayload(0, {
        ok: false,
        code: 'ERR_USER_CODE',
        name: 'WorkflowSuspend',
        message: 'suspend',
        fields: { kind: 'waitForEvent', attempt: 2 },
      }),
    )
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.fields).toEqual({ kind: 'waitForEvent', attempt: 2 })
  })

  test('an absent fields slot decodes as undefined', () => {
    const { result } = decodeRunCompletionPayload(
      encodeCompletionPayload(0, {
        ok: false,
        code: 'ERR_USER_CODE',
        name: 'Error',
        message: 'plain',
      }),
    )
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.fields).toBeUndefined()
  })

  test.each([
    'ERR_USER_CODE',
    'ERR_COMPILE',
    'ERR_CPU_TIMEOUT',
    'ERR_WALL_TIMEOUT',
    'ERR_MEMORY_LIMIT',
    'ERR_MODULE_NOT_FOUND',
    'ERR_EXPORT_NOT_SERIALIZABLE',
    'ERR_EXPORT_TOO_LARGE',
    'ERR_HOST_BRIDGE',
    'ERR_BRIDGE_PAYLOAD_TOO_LARGE',
    'ERR_BRIDGE_CALL_LIMIT_EXCEEDED',
    'ERR_UNDECLARED_BINDING',
    'ERR_PREFIX_DISPOSED',
    'ERR_ABORTED',
    'ERR_INTERNAL',
  ])('error code %s round-trips', (code) => {
    const { result } = decodeRunCompletionPayload(
      encodeCompletionPayload(0, { ok: false, code, name: 'Error', message: 'm' }),
    )
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.code).toBe(code as RunErrorCode)
  })
})

// ── decodeRunCompletionPayload — malformed input ───────────────────────────

describe('decodeRunCompletionPayload — errors', () => {
  test('empty buffer', () => {
    expect(() => decodeRunCompletionPayload(new Uint8Array())).toThrow(PayloadDecodeError)
  })

  test('truncated after runId', () => {
    expect(() => decodeRunCompletionPayload(u32(1))).toThrow(PayloadDecodeError)
  })

  test('invalid bool byte in the ok field', () => {
    expect(() => decodeRunCompletionPayload(Buffer.concat([u32(0), Buffer.from([0x02])])))
      .toThrow(/invalid bool byte/)
  })

  test('trailing bytes after the payload', () => {
    const buf = Buffer.concat([
      encodeCompletionPayload(0, { ok: true, exports: {} }),
      Buffer.from([0xDE, 0xAD]),
    ])
    expect(() => decodeRunCompletionPayload(buf)).toThrow(/trailing bytes/)
  })

  test('exports that do not decode to an object', () => {
    const buf = encodeCompletionPayload(0, { ok: true, exports: [1, 2, 3] })
    expect(() => decodeRunCompletionPayload(buf)).toThrow(/exports must decode to an object/)
  })
})

// ── decodePrecompileResultPayload ──────────────────────────────────────────

describe('decodePrecompileResultPayload', () => {
  function encodeSuccess(prefixId: string): Buffer {
    return Buffer.concat([Buffer.from([1, 1]), str(prefixId), Buffer.from([0])])
  }

  function encodeFailure(code: string, message: string, stack?: string): Buffer {
    return Buffer.concat([
      Buffer.from([0, 0, 1]),
      str(code),
      str('SyntaxError'),
      str(message),
      optionalString(stack),
      Buffer.from([0]), // fields absent
    ])
  }

  test('success decodes prefixId', () => {
    const result = decodePrecompileResultPayload(encodeSuccess('42'))
    expect(result.ok).toBe(true)
    if (!result.ok)
      return
    expect(result.prefixId).toBe('42')
  })

  test('failure decodes error fields', () => {
    const result = decodePrecompileResultPayload(
      encodeFailure('ERR_COMPILE', 'bad syntax', 'at line 1'),
    )
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.code).toBe('ERR_COMPILE')
    expect(result.error.message).toBe('bad syntax')
    expect(result.error.stack).toBe('at line 1')
  })

  test('failure without a stack', () => {
    const result = decodePrecompileResultPayload(encodeFailure('ERR_USER_CODE', 'boom'))
    expect(result.ok).toBe(false)
    if (result.ok)
      return
    expect(result.error.stack).toBeUndefined()
  })
})

// ── decodeBridgeCallPayload ────────────────────────────────────────────────

describe('decodeBridgeCallPayload', () => {
  test('the whole argument list arrives in one blob and is spread', () => {
    const call = decodeBridgeCallPayload(
      encodeBridgeCallPayload(7, 'greet', ['world', 42, { deep: true }]),
    )
    expect(call).toEqual({
      callId: 7,
      targetKind: 0,
      specifier: undefined,
      exportName: 'greet',
      args: ['world', 42, { deep: true }],
    })
  })

  test('no arguments decodes to an empty list', () => {
    const call = decodeBridgeCallPayload(encodeBridgeCallPayload(0, 'ping', []))
    expect(call.args).toEqual([])
  })

  test('an import target carries its specifier and leaf path', () => {
    const call = decodeBridgeCallPayload(
      encodeBridgeCallPayload(1, 'nested.inner', [1], 'tools:search'),
    )
    expect(call.targetKind).toBe(1)
    expect(call.specifier).toBe('tools:search')
    expect(call.exportName).toBe('nested.inner')
  })

  test('arguments sharing one object keep their identity', () => {
    const shared = { x: 1 }
    const call = decodeBridgeCallPayload(encodeBridgeCallPayload(0, 'f', [shared, shared]))
    expect(call.args[0]).toBe(call.args[1])
  })

  test('rejects an args blob that does not hold an array', () => {
    const buf = Buffer.concat([
      u32(0),
      Buffer.from([0, 0]),
      str('f'),
      valueSlot({ notAnArray: true }),
    ])
    expect(() => decodeBridgeCallPayload(buf)).toThrow(/must decode to an array/)
  })

  test('rejects an invalid targetKind', () => {
    const buf = Buffer.concat([u32(0), Buffer.from([9, 0]), str('f'), valueSlot([])])
    expect(() => decodeBridgeCallPayload(buf)).toThrow(/invalid bridge targetKind/)
  })
})
