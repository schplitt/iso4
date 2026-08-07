/**
 * Deterministic benchmark payloads, shared by all bench suites.
 *
 * Codec cost tracks **value count, not byte size**, so the matrix is chosen
 * by shape rather than size alone (mirrored by `native/v8-runtime/benches/`):
 *
 * - `sparse1k` — ~10 keys, long strings, ~750 B. A realistic analytics event.
 * - `dense1k`  — ~200 values, ~1.3 KB. Value-dense but small.
 * - `dense2m`  — 12k rows × 4 fields, ~0.5–0.7 MB. Value-dense and large;
 *                the worst case for per-value codec overhead (~48k values).
 * - `bytes2m`  — one 2 MB Uint8Array. Exercises the bytes plane only.
 *
 * All data is generated from a fixed-seed PRNG so every run (and both the
 * base and head side of an A/B comparison) benches identical bytes.
 *
 * Factories return `HostExportData` — the codec-safe recursive data type —
 * so the fixtures are assignable to data globals without casts.
 */

import type { HostExportData } from '../src/index.js'

/**
 * Mulberry32 - tiny deterministic PRNG. Quality is irrelevant here; only
 * determinism and speed matter.
 * @param seed fixed seed; one seed per payload shape
 */
function prng(seed: number): () => number {
  let s = seed >>> 0
  return () => {
    s = (s + 0x6D2B79F5) >>> 0
    let t = s
    t = Math.imul(t ^ (t >>> 15), t | 1)
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61)
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296
  }
}

const WORDS = [
  'checkout',
  'session',
  'purchase',
  'pageview',
  'signup',
  'tenant',
  'campaign',
  'variant',
  'mobile',
  'desktop',
]

function sentence(rand: () => number, words: number): string {
  const parts: string[] = []
  for (let i = 0; i < words; i++) {
    parts.push(WORDS[Math.floor(rand() * WORDS.length)] as string)
  }
  return parts.join(' ')
}

/**
 * ~10 keys, long string values, ~750 B — realistic analytics event.
 */
export function sparse1k(): Record<string, HostExportData> {
  const rand = prng(0x1504)
  return {
    eventId: `evt_${Math.floor(rand() * 1e9).toString(36)}`,
    tenantId: 'tenant_4c1f9a2b',
    type: 'analytics.pageview',
    url: `https://app.example.com/dashboards/${Math.floor(rand() * 1e6)}?utm_source=newsletter&utm_campaign=q3-launch`,
    userAgent:
      'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36',
    referrer: `https://www.example.com/search?q=${sentence(rand, 8).replaceAll(' ', '+')}`,
    description: sentence(rand, 40),
    timestamp: 1722945600000 + Math.floor(rand() * 86400000),
    sessionDurationMs: rand() * 900000,
    isAuthenticated: true,
  }
}

/**
 * ~200 values, ~1.3 KB — value-dense but small.
 */
export function dense1k(): Record<string, HostExportData> {
  const rand = prng(0xD513)
  const metrics: Record<string, number> = {}
  for (let i = 0; i < 96; i++) {
    metrics[`m${i}`] = rand() * 1000
  }
  const tags: string[] = []
  for (let i = 0; i < 48; i++) {
    tags.push(WORDS[Math.floor(rand() * WORDS.length)] as string)
  }
  const flags: boolean[] = []
  for (let i = 0; i < 48; i++) {
    flags.push(rand() > 0.5)
  }
  return { kind: 'metrics.batch', metrics, tags, flags }
}

/**
 * 12k rows × 4 fields (~48k values, ~0.5–0.7 MB) — value-dense and large.
 */
export function dense2m(): Record<string, HostExportData> {
  const rand = prng(0xDE2E)
  const rows: Record<string, HostExportData>[] = []
  for (let i = 0; i < 12000; i++) {
    rows.push({
      id: i,
      name: `${WORDS[i % WORDS.length]}_${Math.floor(rand() * 1e6).toString(36)}`,
      value: rand() * 10000,
      active: rand() > 0.3,
    })
  }
  return { kind: 'rows.batch', rows }
}

/**
 * One 2 MB Uint8Array — bytes plane.
 */
export function bytes2m(): Uint8Array {
  const rand = prng(0xB2E5)
  const buf = new Uint8Array(2 * 1024 * 1024)
  // Fill 4 bytes per PRNG draw; per-byte draws would dominate fixture setup.
  for (let i = 0; i < buf.length; i += 4) {
    const n = Math.floor(rand() * 4294967296)
    buf[i] = n & 0xFF
    buf[i + 1] = (n >>> 8) & 0xFF
    buf[i + 2] = (n >>> 16) & 0xFF
    buf[i + 3] = (n >>> 24) & 0xFF
  }
  return buf
}

export interface PayloadShape {
  name: 'sparse1k' | 'dense1k' | 'dense2m' | 'bytes2m'
  make: () => Record<string, HostExportData> | Uint8Array
}

/**
 * The payload matrix as iterable entries for suite loops.
 * Values are factories so each suite decides when to materialise (fixtures
 * are built once at module load in each bench file, not per iteration).
 */
export const PAYLOAD_MATRIX: readonly PayloadShape[] = [
  { name: 'sparse1k', make: sparse1k },
  { name: 'dense1k', make: dense1k },
  { name: 'dense2m', make: dense2m },
  { name: 'bytes2m', make: bytes2m },
]
