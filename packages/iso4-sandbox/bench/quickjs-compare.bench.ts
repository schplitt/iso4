/**
 * Small end-to-end comparison with QuickJS compiled to WebAssembly.
 *
 * The cases intentionally measure the same broad costs as runtime.bench.ts:
 * cold startup plus evaluation, and a hot evaluation/round trip on a resident
 * runtime. QuickJS runs in-process; iso4 includes its Unix-socket round trip.
 */

import { afterAll, bench, describe } from 'vitest'
import { getQuickJS, newQuickJSWASMModule } from 'quickjs-emscripten'
import type { QuickJSContext, QuickJSRuntime, QuickJSWASMModule } from 'quickjs-emscripten'
import { createSandbox } from '../src/index.js'
import type { Sandbox } from '../src/index.js'
import { HEAVY_OPTS } from './profile.js'

const ISO4_CODE = 'export default 42'
const QUICKJS_CODE = '42'

const iso4: Sandbox = await createSandbox({ maxIsolates: 1 })
const quickjsModule: QuickJSWASMModule = await getQuickJS()
const quickjsRuntime: QuickJSRuntime = quickjsModule.newRuntime()
const quickjs: QuickJSContext = quickjsRuntime.newContext()

afterAll(async () => {
  quickjs.dispose()
  quickjsRuntime.dispose()
  await iso4.dispose()
})

describe('cold startup + evaluation', () => {
  bench(
    'iso4 (spawn → connect → run → dispose)',
    async () => {
      const runtime = await createSandbox({ maxIsolates: 1 })
      const result = await runtime.run({ code: ISO4_CODE })
      if (!result.ok || result.exports.default !== 42)
        throw new Error('iso4 cold benchmark sanity check failed')
      await runtime.dispose()
    },
    { ...HEAVY_OPTS, warmupIterations: 0 },
  )

  bench(
    'QuickJS Emscripten (WASM module → context → eval → dispose)',
    async () => {
      const module = await newQuickJSWASMModule()
      const runtime = module.newRuntime()
      const context = runtime.newContext()
      const result = context.unwrapResult(context.evalCode(QUICKJS_CODE))
      if (context.dump(result) !== 42)
        throw new Error('QuickJS cold benchmark sanity check failed')
      result.dispose()
      context.dispose()
      runtime.dispose()
    },
    { ...HEAVY_OPTS, warmupIterations: 0 },
  )
})

describe('hot evaluation + round trip', () => {
  bench(
    'iso4 (resident process → run → socket round trip)',
    async () => {
      const result = await iso4.run({ code: ISO4_CODE })
      if (!result.ok || result.exports.default !== 42)
        throw new Error('iso4 hot benchmark sanity check failed')
    },
    HEAVY_OPTS,
  )

  bench(
    'QuickJS Emscripten (resident context → eval)',
    () => {
      const result = quickjs.unwrapResult(quickjs.evalCode(QUICKJS_CODE))
      if (quickjs.dump(result) !== 42)
        throw new Error('QuickJS hot benchmark sanity check failed')
      result.dispose()
    },
    HEAVY_OPTS,
  )
})
