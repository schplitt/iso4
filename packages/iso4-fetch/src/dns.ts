/**
 * DNS for the connection-time pinning interceptor, plus the private/reserved-IP
 * block that runs before undici connects.
 *
 * This is the ONLY place a hostname is resolved. Resolution happens inside the
 * undici DNS interceptor, which runs only when a request actually connects —
 * i.e. after it has passed the allow/deny check. A denied host is therefore
 * never handed to a resolver, so the allow-list also gates DNS, not just the
 * HTTP request.
 *
 * Not re-exported from `index.ts`, so none of this is public API.
 */

import { lookup as nodeDnsLookupCb } from 'node:dns'
import type { interceptors } from 'undici'

/**
 * True for addresses that must never be connected to from guest-driven fetch:
 * loopback, link-local, RFC1918 / ULA, IMDS, and other reserved ranges. A
 * malformed address is treated as reserved (fail closed).
 * @param ip dotted-quad or IPv6 literal, brackets tolerated
 */
export function isReservedIp(ip: string): boolean {
  if (ip.includes(':')) {
    const addr = ip.toLowerCase().replace(/^\[|\]$/g, '')
    if (addr === '::1')
      return true
    // The unspecified address. Connecting to it reaches the local host.
    if (addr === '::')
      return true
    const firstWord = parseInt(addr.split(':')[0] ?? '0', 16)
    if ((firstWord & 0xffc0) === 0xfe80)
      return true // fe80::/10 link-local
    if ((firstWord & 0xfe00) === 0xfc00)
      return true // fc00::/7 unique-local
    if (addr.startsWith('::ffff:'))
      return isReservedIp(addr.slice(7))
    return false
  }

  const parts = ip.split('.').map(Number)
  if (parts.length !== 4 || parts.some((p) => Number.isNaN(p) || p < 0 || p > 255))
    return true

  const a = parts[0]!
  const b = parts[1] ?? 0
  const c = parts[2] ?? 0

  return (
    a === 0
    || a === 10
    || a === 127
    || (a === 100 && (b & 0xc0) === 64)
    || (a === 169 && b === 254)
    || (a === 172 && b >= 16 && b <= 31)
    || (a === 192 && b === 0 && (c === 0 || c === 2))
    || (a === 192 && b === 168)
    || (a === 198 && (b === 18 || b === 19))
    || (a === 198 && b === 51 && c === 100)
    || (a === 203 && b === 0 && c === 113)
    || a >= 224
  )
}

/**
 * Throw if a hostname resolved to no usable public address. Exported for tests:
 * the undici interceptor is bypassed when `fetch` is mocked, so this is where
 * the SSRF block is exercised directly.
 * @param hostname the hostname that was resolved (for the error message)
 * @param addresses the resolved addresses
 */
export function assertPublicAddresses(
  hostname: string,
  addresses: ReadonlyArray<{ address: string }>,
): void {
  if (addresses.length === 0)
    throw new Error(`fetch: no DNS addresses for "${hostname}"`)
  for (const { address } of addresses) {
    if (isReservedIp(address))
      throw new Error(`fetch: request blocked — "${hostname}" resolves to private/reserved IP ${address}`)
  }
}

/**
 * The lookup function for undici's DNS interceptor: resolve the hostname and
 * refuse any private/reserved address before a connection is made.
 */
export function makeDnsLookupFn(): NonNullable<NonNullable<Parameters<typeof interceptors.dns>[0]>['lookup']> {
  return (hostnameOrUrl, _opts, callback) => {
    const rawHostname: string
      = typeof hostnameOrUrl === 'object' && hostnameOrUrl !== null
        ? (hostnameOrUrl as unknown as URL).hostname
        : hostnameOrUrl

    // `URL.hostname` keeps the brackets around an IPv6 literal and
    // `dns.lookup('[::1]')` cannot parse them, so a literal IPv6 URL failed to
    // resolve at all. Strip them; `dns.lookup` hands a bare literal straight
    // back.
    const hostname = rawHostname.startsWith('[') && rawHostname.endsWith(']')
      ? rawHostname.slice(1, -1)
      : rawHostname

    nodeDnsLookupCb(hostname, { all: true, family: 0 }, (err, addresses) => {
      if (err) {
        callback(err, [])
        return
      }
      try {
        assertPublicAddresses(hostname, addresses)
      } catch (blockErr) {
        callback(blockErr as Error, [])
        return
      }
      callback(null, addresses.map((addr) => ({ address: addr.address, family: addr.family as 4 | 6, ttl: 10_000 })))
    })
  }
}
