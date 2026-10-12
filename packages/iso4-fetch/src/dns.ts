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
import { isIPv6 } from 'node:net'
import { buildConnector } from 'undici'
import type { interceptors } from 'undici'

/**
 * True for addresses that must never be connected to from guest-driven fetch:
 * loopback, link-local, RFC1918 / ULA, IMDS, and other reserved ranges. A
 * malformed address is treated as reserved (fail closed).
 * @param ip dotted-quad or IPv6 literal, brackets tolerated
 */
export function isReservedIp(ip: string): boolean {
  if (ip.includes(':')) {
    const addr = ip.toLowerCase().replace(/^\[|\]$/g, '').replace(/%.*$/, '')
    if (!isIPv6(addr))
      return true
    // Move a trailing dotted IPv4 into two hex words, then expand `::`.
    const hex = addr.replace(/(\d+)\.(\d+)\.(\d+)\.(\d+)$/, (_m, a, b, c, d) =>
      `${(Number(a) * 256 + Number(b)).toString(16)}:${(Number(c) * 256 + Number(d)).toString(16)}`)
    const [head = '', tail] = hex.split('::')
    const headWords = head === '' ? [] : head.split(':')
    const tailWords = tail === undefined || tail === '' ? [] : tail.split(':')
    const w = [...headWords, ...Array.from<string>({ length: 8 - headWords.length - tailWords.length }).fill('0'), ...tailWords]
      .map((word) => parseInt(word, 16))
    const v4 = (hi: number, lo: number): string => `${hi >> 8}.${hi & 0xff}.${lo >> 8}.${lo & 0xff}`

    // Embedded IPv4 is judged as that IPv4. `::/96` also covers `::` and `::1`.
    if (w[0] === 0 && w[1] === 0 && w[2] === 0 && w[3] === 0 && w[4] === 0 && (w[5] === 0 || w[5] === 0xffff))
      return isReservedIp(v4(w[6]!, w[7]!)) // ::/96 compatible, ::ffff:0:0/96 mapped
    if (w[0] === 0 && w[1] === 0 && w[2] === 0 && w[3] === 0 && w[4] === 0xffff && w[5] === 0)
      return isReservedIp(v4(w[6]!, w[7]!)) // ::ffff:0:0:0/96 SIIT
    if (w[0] === 0x64 && w[1] === 0xff9b && w[2] === 0 && w[3] === 0 && w[4] === 0 && w[5] === 0)
      return isReservedIp(v4(w[6]!, w[7]!)) // 64:ff9b::/96 NAT64
    if (w[0] === 0x2002)
      return isReservedIp(v4(w[1]!, w[2]!)) // 2002::/16 6to4

    const first = w[0]!
    return (
      (first === 0x64 && w[1] === 0xff9b && w[2] === 1) // 64:ff9b:1::/48 local-use NAT64
      || (first === 0x2001 && w[1] === 0) // 2001::/32 Teredo
      || (first & 0xffc0) === 0xfe80 // fe80::/10 link-local
      || (first & 0xffc0) === 0xfec0 // fec0::/10 site-local (deprecated)
      || (first & 0xfe00) === 0xfc00 // fc00::/7 unique-local
      || (first & 0xff00) === 0xff00 // ff00::/8 multicast
    )
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

/**
 * undici connector that drops a socket whose actual remote address is reserved.
 * Catches whatever the literal and DNS checks miss, before any request bytes are sent.
 */
export function makeGuardedConnector(): buildConnector.connector {
  const connect = buildConnector({})
  return (opts, callback) => {
    connect(opts, (err, socket) => {
      if (err !== null) {
        callback(err, null)
        return
      }
      const address = socket.remoteAddress ?? ''
      if (isReservedIp(address)) {
        socket.destroy()
        callback(new Error(`fetch: request blocked — "${opts.hostname}" connected to private/reserved IP ${address}`), null)
        return
      }
      callback(null, socket)
    })
  }
}
