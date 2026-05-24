import { expect, test } from 'vitest'
import { createStaticRuntime } from '../src/index.js'

test('createStaticRuntime is exported and not yet implemented', async () => {
  await expect(createStaticRuntime()).rejects.toThrow(/not yet implemented/)
})
