# @iso4/fetch

## 0.0.2

### Patch Changes

- 7fb2cde: fix: resolve DNS only for authorized requests

  DNS is no longer resolved before the allow/deny check, so a denied host is
  never looked up — closing a covert-lookup channel and an internal-network
  oracle. The private/reserved-IP block now runs at connection time (for allowed
  requests only) and `SafeFetchRequest.resolvedIp` is always `null`.

- 5ca5ab9: fix: reach origins that resolve to IPv6

  DNS pinning forced IPv4, so an IPv6-only host was unreachable and could end
  the host process. Literal IPv6 URLs now resolve too, and `::` is treated as
  reserved.

- 939afdd: fix: a matched host claims the origin for scheme and port

  A request whose host matches a rule but uses a disallowed scheme or port is now
  denied, matching the route behaviour, instead of falling through to the
  `policy` callback.

- 1c8cc0a: fix: match request paths literally instead of decoding them

  The route allowlist now matches the path exactly as it is sent, with no
  percent-decoding, so an encoded slash can no longer make the matched path differ
  from the path on the wire. `.`/`..` are still normalised by the URL parser.

- fd0cd30: fix: strip credentials on cross-origin redirects

  Following a redirect to a different origin now drops `authorization`, `cookie`
  and `proxy-authorization`, and a method-changing redirect drops `content-*`
  headers, matching undici's redirect handling.

- c501131: fix: `setBody` no longer depends on `this`

  `ctx.req.setBody` can now be destructured or passed as a callback without
  throwing; it writes the request body through a closure like `header` and
  `setUrl`.

- d544d5b: fix: `timeoutMs` bounds the whole request, not each redirect hop

  A single deadline now spans the entire redirect chain, so a redirecting request
  can no longer run for `(maxRedirects + 1)` times the configured timeout.

## 0.0.1

### Patch Changes

- 88554a4: initial release
