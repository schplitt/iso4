import { fileURLToPath } from 'node:url'
import { dirname, join } from 'node:path'

const __dirname = dirname(fileURLToPath(import.meta.url))

export const binaryPath = join(__dirname, 'bin', 'iso4-v8')
