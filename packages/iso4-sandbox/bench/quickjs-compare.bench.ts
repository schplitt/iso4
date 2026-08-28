/**
 * Resident-runtime comparison with QuickJS compiled to WebAssembly.
 *
 * Runtime creation and prefix warm-up happen before the timed benches. This
 * compares QuickJS evaluation with iso4's direct run and warm prefix paths.
 */

import { afterAll, bench, describe } from 'vitest'
import { getQuickJS } from 'quickjs-emscripten'
import type { QuickJSContext, QuickJSRuntime, QuickJSWASMModule } from 'quickjs-emscripten'
import { createSandbox } from '../src/index.js'
import type { Prefix, Sandbox } from '../src/index.js'
import { HEAVY_OPTS } from './profile.js'

const ISO4_CODE = 'export default 42'
const QUICKJS_CODE = '42'

const iso4: Sandbox = await createSandbox({ maxIsolates: 1 })
const prefix: Prefix = await iso4.precompile({ code: 'export default () => 42' })
const prefixWarmup = await prefix.call({ export: 'default', args: [] })
if (!prefixWarmup.ok || prefixWarmup.value !== 42)
  throw new Error('iso4 prefix warm-up sanity check failed')

const quickjsModule: QuickJSWASMModule = await getQuickJS()
const quickjsRuntime: QuickJSRuntime = quickjsModule.newRuntime()
const quickjs: QuickJSContext = quickjsRuntime.newContext()

afterAll(async () => {
  quickjs.dispose()
  quickjsRuntime.dispose()
  await iso4.dispose()
})

describe('resident runtime evaluation + round trip', () => {
  bench(
    'iso4 sandbox.run (resident process → run → socket round trip)',
    async () => {
      const result = await iso4.run({ code: ISO4_CODE })
      if (!result.ok || result.exports.default !== 42)
        throw new Error('iso4 sandbox.run sanity check failed')
    },
    HEAVY_OPTS,
  )

  bench(
    'iso4 sandbox.prefix (warm isolate → call → socket round trip)',
    async () => {
      const result = await prefix.call({ export: 'default', args: [] })
      if (!result.ok || result.value !== 42)
        throw new Error('iso4 sandbox.prefix sanity check failed')
    },
    HEAVY_OPTS,
  )

  bench(
    'QuickJS (resident context → eval)',
    () => {
      const result = quickjs.unwrapResult(quickjs.evalCode(QUICKJS_CODE))
      if (quickjs.dump(result) !== 42)
        throw new Error('QuickJS sanity check failed')
      result.dispose()
    },
    HEAVY_OPTS,
  )
})
