import { expect, test } from 'vitest'
import { createSafeFetch } from '../src/index'
import type { SafeFetchPolicy } from '../src/index'

test('createSafeFetch returns a handler that throws (not yet implemented)', async () => {
  const policy: SafeFetchPolicy = () => true
  const handler = createSafeFetch({ policy })
  await expect(
    handler({
      url: 'https://example.com/',
      method: 'GET',
      headers: {},
      body: null,
      signal: new AbortController().signal,
    }),
  ).rejects.toThrow(/not yet implemented/)
})

test('createSafeFetch accepts the documented option shape without throwing at construction', () => {
  const policy: SafeFetchPolicy = ({ host, method, path, hop }) => {
    if (host !== 'api.example.com')
      return false
    if (method !== 'GET' && method !== 'POST') {
      throw new Error(`method ${method} not allowed`)
    }
    if (hop > 0 && path.startsWith('/admin')) {
      throw new Error('redirect to /admin denied')
    }
    return true
  }

  expect(() =>
    createSafeFetch({
      policy,
      pinDns: true,
      maxRedirects: 0,
      timeoutMs: 5_000,
      maxBodyBytes: 1024 * 1024,
      allowCompressedResponses: false,
      onRequest: () => {},
      onDenied: () => {},
    }),
  ).not.toThrow()
})

test('policy may be async', () => {
  const policy: SafeFetchPolicy = async ({ host }) => {
    return host === 'api.example.com'
  }
  expect(() => createSafeFetch({ policy })).not.toThrow()
})
