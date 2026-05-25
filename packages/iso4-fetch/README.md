# @iso4/fetch

Hardened `FetchHandler` for the [iso4](../iso4) sandbox.

Every request flows through a host-supplied **policy callback** that decides
allow or deny. Deny reasons can be opaque or explicit — your choice per
request. On top of that the package ships mitigations for SSRF, DNS
rebinding, redirect bypass, and response amplification.

> **Status:** scaffolding. The factory exists and types are stable, but the
> returned handler currently throws on every call. Implementation lands in
> build-plan phase 5 (see [`../../DESIGN.md`](../../DESIGN.md) §9).

## Install

```sh
npm i iso4 @iso4/fetch
```

`iso4` is a peer dependency.

## Usage

```ts
import { createRuntime } from 'iso4'
import { createSafeFetch } from '@iso4/fetch'
import type { SafeFetchPolicy } from '@iso4/fetch'

const policy: SafeFetchPolicy = ({ host, path, method, hop, resolvedIp }) => {
  // Hard-deny anything off the allowed origin.
  if (host !== 'api.example.com')
return false

  // Method-level restriction with an explicit reason for the sandbox.
  if (method !== 'GET' && method !== 'POST') {
    throw new Error(`method ${method} not permitted`)
  }

  // Carve-out: refuse admin paths even if the rest of the policy would allow.
  if (path.startsWith('/admin')) {
    throw new Error('admin endpoints denied')
  }

  // Refuse redirects to anywhere other than the original host.
  if (hop > 0 && host !== 'api.example.com') {
    throw new Error('cross-origin redirect denied')
  }

  // Belt-and-suspenders: if DNS pinning resolved to a private IP, deny.
  if (resolvedIp?.startsWith('192.168.') || resolvedIp?.startsWith('10.')) {
    throw new Error('private IP denied')
  }

  return true
}

const runtime = await createRuntime()
const prefix = await runtime.precompile({
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

Return value semantics:

| Return                             | Result                                                |
| ---------------------------------- | ----------------------------------------------------- |
| `true`                             | Allow                                                 |
| `false`                            | Deny with generic reason `"request denied by policy"` |
| `throw new Error("custom reason")` | Deny; the thrown message surfaces to sandbox          |
| `Promise<...>` of any of the above | Same, awaited                                         |

The sandbox-side `fetch()` Promise rejects with an `Error` whose
`.message` is the deny reason. The host therefore controls how much detail
leaks back into the sandbox.

## What it protects against

Mitigations layered on top of the mechanical hygiene `iso4` core already
enforces:

| Attack                                   | Mitigation                                                                       |
| ---------------------------------------- | -------------------------------------------------------------------------------- |
| SSRF to internal services / metadata IPs | DNS pre-resolution (`pinDns`) + policy sees `resolvedIp` to enforce ranges       |
| DNS rebinding                            | DNS pinned at request time; underlying fetch goes to the resolved IP             |
| Redirect-based bypass                    | No auto-redirect by default; with `maxRedirects > 0`, policy re-runs per hop     |
| Response-size amplification              | `maxBodyBytes` enforced pre-decompression; compressed responses off by default   |
| Host-app HTTP pool / auth leakage        | Uses an isolated `undici` Dispatcher with no shared cookies, auth, or middleware |
| Method-based attacks                     | Policy can enforce method allowlists                                             |

See [`../../DESIGN.md`](../../DESIGN.md) §12 for the full threat model.

## What it does NOT protect against

- **Policy mistakes.** If your `policy` allows the wrong host, this package
  won't second-guess you.
- **Timing oracles.** Denial latency is roughly constant but not
  cryptographically normalized.
- **Misuse by the host author.** If you bypass `createSafeFetch` and wire
  your own handler to the sandbox, the protections from this package don't
  apply.

## License

MIT
