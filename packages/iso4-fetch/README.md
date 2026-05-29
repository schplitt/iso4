# @iso4/fetch

Hardened `FetchHandler` for [@iso4/sandbox](../iso4-sandbox).

Every request flows through a host-supplied **policy callback** that decides
allow or deny. On top of that the package ships mitigations for SSRF, DNS
rebinding, redirect bypass, and response amplification.

> **Status:** scaffolding. Types and option surface are stable, but the
> returned handler currently throws on every call. Implementation lands in
> build-plan phase 5 (see [DESIGN.md](../../DESIGN.md) §9).

## Install

```sh
npm i @iso4/sandbox @iso4/fetch
```

## Usage

```ts
import { createSandbox } from '@iso4/sandbox'
import { createSafeFetch } from '@iso4/fetch'
import type { SafeFetchPolicy } from '@iso4/fetch'

const policy: SafeFetchPolicy = ({ host, path, method, hop, resolvedIp }) => {
  if (host !== 'api.example.com')
return false

  if (method !== 'GET' && method !== 'POST')
    throw new Error(`method ${method} not permitted`)

  if (path.startsWith('/admin'))
    throw new Error('admin endpoints denied')

  // Re-runs on every redirect hop
  if (hop > 0 && host !== 'api.example.com')
    throw new Error('cross-origin redirect denied')

  return true
}

const sandbox = await createSandbox()
const prefix = await sandbox.precompile({
  code: `/* host setup */`,
  globals: {
    fetch: createSafeFetch({
      policy,
      pinDns: true,
      maxRedirects: 0,
      timeoutMs: 5_000,
    }),
  },
})
```

## The policy callback

The single mechanism for allow/deny. Receives a normalized
[`SafeFetchRequest`](./src/types.ts) for every outbound call:

```ts
interface SafeFetchRequest {
  url: string // canonical URL
  protocol: 'http' | 'https'
  host: string // hostname only
  port: number // 80/443 implicit
  path: string // /path?query
  method: string // uppercased
  headers: Record<string, string> // names lowercased
  resolvedIp: string | null // when pinDns is on
  hop: number // 0 = initial, 1+ = redirect
}
```

| Return                             | Result                            |
| ---------------------------------- | --------------------------------- |
| `true`                             | Allow                             |
| `false`                            | Deny with generic reason          |
| `throw new Error("reason")`        | Deny; message surfaces to sandbox |
| `Promise<...>` of any of the above | Same, awaited                     |

## What it protects against

| Attack                        | Mitigation                                                   |
| ----------------------------- | ------------------------------------------------------------ |
| SSRF to internal/metadata IPs | DNS pre-resolution + policy sees `resolvedIp`                |
| DNS rebinding                 | DNS pinned at request time, request goes to resolved IP      |
| Redirect-based bypass         | No auto-redirect by default; policy re-runs per hop          |
| Response-size amplification   | `maxBodyBytes` enforced; compressed responses off by default |
| Host-app auth leakage         | Isolated `undici` Dispatcher — no shared cookies or auth     |

See [DESIGN.md](../../DESIGN.md) §12 for the full threat model.

## License

MIT
