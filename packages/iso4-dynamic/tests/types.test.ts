import { expect, test } from 'vitest'
import { createRuntime } from '../src/index.js'

test('createRuntime is exported and not yet implemented', async () => {
  await expect(createRuntime()).rejects.toThrow(/not yet implemented/)
})
