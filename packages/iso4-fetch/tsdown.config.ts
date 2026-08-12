import { defineConfig } from 'tsdown'
import ApiSnapshot from 'tsnapi/rolldown'

export default defineConfig({
  entry: {
    index: './src/index.ts',
  },
  target: ['es2024'],
  format: 'esm',
  clean: true,
  dts: true,
  outDir: './dist',
  // `iso4` is a peer dep; do not bundle its types in.
  external: ['iso4'],
  plugins: [
    ApiSnapshot(),
  ],
})
