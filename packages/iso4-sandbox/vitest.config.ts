import codspeedPlugin from '@codspeed/vitest-plugin'
import { defineConfig } from 'vitest/config'

// CodSpeed instrumentation for `vitest bench` — see
// .github/workflows/codspeed.yml. The plugin is a no-op outside the CodSpeed
// runner, so local test and bench runs are unaffected.
export default defineConfig({
  plugins: [codspeedPlugin()],
})
