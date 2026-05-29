/**
 * \@iso4/fetch — FetchHandler types and option/policy types for hardened fetch.
 *
 * The package's job is to produce a `FetchHandler` (defined in `iso4`) that
 * delegates allow/deny decisions to a host-supplied **policy callback**.
 * The callback receives a normalized request descriptor; the host returns
 * `true` to allow, `false` to deny with a generic reason, or throws an
 * `Error` to deny with a custom reason that surfaces to sandbox code.
 *
 * See `../../../DESIGN.md` §12 for the threat model and the responsibility
 * split between `iso4` (mechanical hygiene), this package (hardened
 * defaults), and host application code (policy).
 */

/**
 * Normalized request descriptor passed to a `SafeFetchPolicy`.
 *
 * All fields are derived from the sandbox-provided request after WHATWG
 * URL canonicalization and header validation. The shape is stable: future
 * additions are optional fields, never breaking changes to existing ones.
 */
// ─────────────────────────────────────────────────────────────────────────
// FetchHandler — the typed interface for a fetch-compatible bridge handler.
// These types live here, not a core bridge concern, because fetch handling is not
// a core bridge concern. The core bridge is generic: HostExportFunction.
// FetchHandler is a convenience wrapper with fetch-shaped request/response.
// ─────────────────────────────────────────────────────────────────────────

/**
 * The request object a FetchHandler receives. Reflects the sandbox's
 * `fetch(url, init)` call after URL parsing and header normalisation.
 */
export interface HostFetchRequest {
  url: string
  method: string
  /**
   * Header names are lowercased.
   */
  headers: Record<string, string>
  /**
   * null for bodyless methods.
   */
  body: Uint8Array | string | null
}

export interface HostFetchResponse {
  status: number
  statusText?: string
  headers: Record<string, string>
  body: Uint8Array | string | null
}

/**
 * A typed handler for a `fetch`-compatible global. Implement directly or
 * use `createSafeFetch` from this package.
 *
 * Plug into globals:
 * ```ts
 * globals: { fetch: myHandler }
 * ```
 */
export type FetchHandler = (
  request: HostFetchRequest,
) => Promise<HostFetchResponse> | HostFetchResponse

// ─────────────────────────────────────────────────────────────────────────
// SafeFetch policy types — for createSafeFetch() option surface.
// ─────────────────────────────────────────────────────────────────────────

export interface SafeFetchRequest {
  /**
   * Full canonical URL string, e.g. `"https://api.example.com/users?x=1"`.
   */
  url: string

  /**
   * Protocol without trailing colon. Always `"http"` or `"https"`.
   */
  protocol: 'http' | 'https'

  /**
   * Hostname only — no port, lowercased.
   * Example: `"api.example.com"`.
   */
  host: string

  /**
   * Port number. Defaults to 80 for `http:` and 443 for `https:` when the
   * URL doesn't specify one.
   */
  port: number

  /**
   * Path plus query string, starting with `/`.
   * Example: `"/users/42?include=profile"`.
   */
  path: string

  /**
   * Uppercased HTTP method. Always non-empty.
   */
  method: string

  /**
   * Request headers, names lowercased. Values are strings with CRLF /
   * control chars already rejected by `iso4` core's bridge-side validation.
   */
  headers: Record<string, string>

  /**
   * Resolved IP for `host`, if `@iso4/fetch` performed DNS pre-resolution
   * before invoking the policy. `null` when policy runs pre-resolution.
   * Use this to make policy decisions based on the actual destination
   * rather than the trusted-input hostname.
   */
  resolvedIp: string | null

  /**
   * Redirect hop index. `0` for the initial request, `1+` for each
   * subsequent redirect. The policy is re-run on every hop when
   * `maxRedirects > 0`, so the policy can implement per-hop rules.
   */
  hop: number
}

/**
 * Allow/deny decision for a single request.
 *
 * Semantics:
 *   - Return `true` to allow the request.
 *   - Return `false` to deny with a generic reason (`"request denied by policy"`).
 *   - `throw new Error("custom reason")` to deny with that reason as the
 *     message visible to sandbox code.
 *
 * The sandbox-side `fetch()` Promise rejects with an `Error` whose
 * `.message` is the deny reason. The host therefore controls what level of
 * detail leaks back into the sandbox — return `false` to keep reasons
 * opaque, throw with a message to make them explicit.
 *
 * Async policies are supported; the underlying request waits for the
 * returned Promise to settle before issuing.
 */
export type SafeFetchPolicy = (
  request: SafeFetchRequest,
) => boolean | Promise<boolean>

/**
 * Options for `createSafeFetch`.
 *
 * The `policy` callback is required. All other options have safe defaults.
 */
export interface SafeFetchOptions {
  /**
   * REQUIRED. Decides allow/deny per request.
   *
   * See `SafeFetchPolicy` for the return-value and throwing semantics.
   *
   * The policy is invoked after `iso4` core's bridge-side validation
   * (URL parsing, header sanitization, method check) and after optional
   * DNS pre-resolution by this package. So by the time the policy sees a
   * request, the inputs are guaranteed to be well-formed; the policy is
   * purely a destination-and-shape check, not a parser.
   */
  policy: SafeFetchPolicy

  /**
   * When `true`, this package performs DNS resolution once before
   * invoking `policy` and pins the underlying HTTP request to the
   * resolved IP literal (with explicit `Host:` header). Prevents DNS
   * rebinding attacks.
   *
   * When `false`, the underlying HTTP client resolves DNS itself at
   * request time. Policies see `resolvedIp: null`.
   *
   * @default true
   */
  pinDns?: boolean

  /**
   * Maximum number of HTTP redirects to follow automatically. Each hop
   * re-runs `policy` with `hop` incremented. When the limit is hit, the
   * underlying response (3xx) is returned to the sandbox as-is.
   *
   * @default 0 (do not auto-follow; redirect responses pass through)
   */
  maxRedirects?: number

  /**
   * Per-request timeout in milliseconds. Aborts the underlying HTTP
   * request after this many milliseconds; the sandbox `fetch` rejects.
   *
   * @default 30_000
   */
  timeoutMs?: number

  /**
   * Maximum response body size in bytes, enforced pre-decompression.
   * @default 16 * 1024 * 1024
   */
  maxBodyBytes?: number

  /**
   * Whether to allow `Content-Encoding: gzip` / `br` / `deflate`
   * decompression. Disabled by default — compression-amplification is an
   * attack vector and the sandbox can request `Accept-Encoding: identity`.
   *
   * @default false
   */
  allowCompressedResponses?: boolean

  /**
   * Optional hook invoked after `policy` allows a request, just before
   * the underlying HTTP call is issued. Useful for structured logging /
   * audit. Throwing from this hook is treated as a denial; the thrown
   * error's message becomes the sandbox-visible deny reason.
   */
  onRequest?: (request: SafeFetchRequest) => void | Promise<void>

  /**
   * Optional hook invoked when a request is denied (by `policy` returning
   * `false`/throwing, or by `onRequest` throwing, or by an internal check
   * like the redirect limit). Useful for alerting on suspicious sandbox
   * behavior. Throwing from this hook is ignored.
   */
  onDenied?: (request: SafeFetchRequest, reason: string) => void | Promise<void>
}
