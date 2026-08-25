// Sandbox-side web runtime. Evaluated once into the context by `webtypes.rs`;
// lands in the prefix snapshot, so it costs nothing per run.
//
// Evaluates to a factory taking the three native shells. The shells are never
// exposed on globalThis — only the classes that extend them are.
//
// Instance state lives in non-enumerable own properties with short names so the
// Rust codec can read it with plain property gets. The names are wire contract:
// keep them in sync with `webcodec.rs`.
//
// Deliberate deviations from spec, all documented in DESIGN.md:
//   - no `.body` getter (that is a ReadableStream; streams cannot cross)
//   - no Blob / FormData body variants
//
// URL parsing is native: `urlParse`/`urlSet` are callbacks backed by the ada
// parser (url.rs), and the URL class below is only the object surface. Like
// the shells, they are never exposed on globalThis.

(function (HeadersShell, RequestShell, ResponseShell, urlParse, urlSet) {
  'use strict'

  const def = (target, name, value) =>
    Object.defineProperty(target, name, {
      value,
      writable: true,
      enumerable: false,
      configurable: true,
    })

  // ── UTF-8 ──────────────────────────────────────────────────────────────────

  function utf8Encode(str) {
    const len = str.length
    // Worst case 3 bytes per UTF-16 unit; surrogate pairs produce 4 bytes for
    // two units, so this bound holds.
    const out = new Uint8Array(len * 3)
    let p = 0
    for (let i = 0; i < len; i++) {
      let c = str.charCodeAt(i)
      if (c < 0x80) {
        out[p++] = c
      } else if (c < 0x800) {
        out[p++] = 0xC0 | (c >> 6)
        out[p++] = 0x80 | (c & 0x3F)
      } else if (c >= 0xD800 && c <= 0xDBFF && i + 1 < len) {
        const next = str.charCodeAt(i + 1)
        if (next >= 0xDC00 && next <= 0xDFFF) {
          c = 0x10000 + ((c - 0xD800) << 10) + (next - 0xDC00)
          i++
          out[p++] = 0xF0 | (c >> 18)
          out[p++] = 0x80 | ((c >> 12) & 0x3F)
          out[p++] = 0x80 | ((c >> 6) & 0x3F)
          out[p++] = 0x80 | (c & 0x3F)
        } else {
          // Lone high surrogate → U+FFFD
          out[p++] = 0xEF; out[p++] = 0xBF; out[p++] = 0xBD
        }
      } else if (c >= 0xD800 && c <= 0xDFFF) {
        out[p++] = 0xEF; out[p++] = 0xBF; out[p++] = 0xBD
      } else {
        out[p++] = 0xE0 | (c >> 12)
        out[p++] = 0x80 | ((c >> 6) & 0x3F)
        out[p++] = 0x80 | (c & 0x3F)
      }
    }
    return out.subarray(0, p)
  }

  // Strict, per the WHATWG encoding spec. The earlier version checked only the
  // lead-byte pattern, so a malformed lead byte consumed the valid characters
  // after it (`41 C3 41` decoded as "AÁ", losing the trailing "A") and overlong
  // and surrogate sequences were accepted. On any error we emit one U+FFFD and
  // advance a single byte, which is what stops the next character being eaten.
  function utf8Decode(bytes) {
    let out = ''
    let buf = []
    let i = 0
    const n = bytes.length

    const flush = () => {
      if (buf.length) {
        out += String.fromCharCode.apply(null, buf)
        buf = []
      }
    }
    const emit = (cp) => {
      if (cp > 0xFFFF) {
        const v = cp - 0x10000
        buf.push(0xD800 + (v >> 10), 0xDC00 + (v & 0x3FF))
      } else {
        buf.push(cp)
      }
      // Chunked: concatenating per code point is O(n^2) on large bodies.
      if (buf.length >= 4096)
        flush()
    }

    while (i < n) {
      const b0 = bytes[i]
      let cp = -1
      let len = 0
      let lower = 0

      if (b0 < 0x80) {
        cp = b0; len = 1; lower = 0
      } else if (b0 >= 0xC2 && b0 <= 0xDF) {
        cp = b0 & 0x1F; len = 2; lower = 0x80
      } else if (b0 >= 0xE0 && b0 <= 0xEF) {
        cp = b0 & 0x0F; len = 3; lower = 0x800
      } else if (b0 >= 0xF0 && b0 <= 0xF4) {
        cp = b0 & 0x07; len = 4; lower = 0x10000
      } else {
        // 0x80-0xC1 is a stray continuation or an overlong lead; 0xF5-0xFF is
        // beyond the Unicode range.
        emit(0xFFFD); i++; continue
      }

      if (i + len > n) {
        emit(0xFFFD); i++; continue
      }
      let ok = true
      for (let k = 1; k < len; k++) {
        const b = bytes[i + k]
        if ((b & 0xC0) !== 0x80) {
          ok = false; break
        }
        cp = (cp << 6) | (b & 0x3F)
      }
      // Reject overlongs, the surrogate range, and anything past U+10FFFF.
      if (!ok || cp < lower || cp > 0x10FFFF || (cp >= 0xD800 && cp <= 0xDFFF)) {
        emit(0xFFFD); i++; continue
      }
      emit(cp)
      i += len
    }
    flush()
    return out
  }

  class TextEncoder {
    get encoding() {
      return 'utf-8'
    }

    encode(input = '') {
      return utf8Encode(String(input))
    }

    // Never writes a partial sequence, and `read` counts the UTF-16 units
    // actually consumed rather than the whole input.
    encodeInto(source, dest) {
      const str = String(source)
      let read = 0
      let written = 0
      for (let i = 0; i < str.length;) {
        const code = str.charCodeAt(i)
        const pair = code >= 0xD800 && code <= 0xDBFF && i + 1 < str.length
          && str.charCodeAt(i + 1) >= 0xDC00 && str.charCodeAt(i + 1) <= 0xDFFF
        const units = pair ? 2 : 1
        const bytes = utf8Encode(str.slice(i, i + units))
        if (written + bytes.length > dest.length)
          break
        dest.set(bytes, written)
        written += bytes.length
        read += units
        i += units
      }
      return { read, written }
    }
  }

  class TextDecoder {
    constructor(label = 'utf-8') {
      const enc = String(label).toLowerCase()
      if (enc !== 'utf-8' && enc !== 'utf8' && enc !== 'unicode-1-1-utf-8')
        throw new RangeError(`TextDecoder: unsupported encoding "${label}"`)
    }

    get encoding() {
      return 'utf-8'
    }

    decode(input) {
      if (input === undefined)
        return ''
      if (input instanceof Uint8Array)
        return utf8Decode(input)
      if (ArrayBuffer.isView(input))
        return utf8Decode(new Uint8Array(input.buffer, input.byteOffset, input.byteLength))
      if (input instanceof ArrayBuffer)
        return utf8Decode(new Uint8Array(input))
      throw new TypeError('TextDecoder.decode: input must be a BufferSource')
    }
  }

  // ── URLSearchParams ────────────────────────────────────────────────────────

  // application/x-www-form-urlencoded: space is "+", not "%20".
  const encodeQueryComponent = (s) =>
    encodeURIComponent(s)
      .replace(/[!'()~]/g, (c) => `%${c.charCodeAt(0).toString(16).toUpperCase()}`)
      .replace(/%20/g, '+')

  class URLSearchParams {
    constructor(init) {
      def(this, '_p', [])
      def(this, '_url', null)
      if (init === undefined || init === null)
        return
      if (typeof init === 'string') {
        this._parse(init)
      } else if (init instanceof URLSearchParams) {
        for (const [k, v] of init._p) this._p.push([k, v])
      } else if (Array.isArray(init)) {
        for (const pair of init) {
          if (pair.length !== 2)
            throw new TypeError('URLSearchParams: each entry must have two elements')
          this._p.push([String(pair[0]), String(pair[1])])
        }
      } else if (typeof init === 'object') {
        for (const k of Object.keys(init)) this._p.push([k, String(init[k])])
      }
    }

    _parse(query) {
      const s = query.charCodeAt(0) === 63 /* ? */ ? query.slice(1) : query
      if (s === '')
        return
      for (const part of s.split('&')) {
        if (part === '')
          continue
        const eq = part.indexOf('=')
        const rawK = eq === -1 ? part : part.slice(0, eq)
        const rawV = eq === -1 ? '' : part.slice(eq + 1)
        this._p.push([decodeQuery(rawK), decodeQuery(rawV)])
      }
    }

    _sync() {
      if (this._url)
        this._url._onSearchChanged()
    }

    append(name, value) {
      this._p.push([String(name), String(value)]); this._sync()
    }

    delete(name) {
      const n = String(name)
      for (let i = this._p.length - 1; i >= 0; i--) {
        if (this._p[i][0] === n)
          this._p.splice(i, 1)
      }
      this._sync()
    }

    get(name) {
      const n = String(name)
      for (const [k, v] of this._p) {
        if (k === n)
          return v
      }
      return null
    }

    getAll(name) {
      const n = String(name)
      return this._p.filter(([k]) => k === n).map(([, v]) => v)
    }

    has(name) {
      const n = String(name)
      return this._p.some(([k]) => k === n)
    }

    // Replaces the FIRST match in place and drops the rest, so parameter order
    // is preserved. Walking backwards kept the last match and reordered.
    set(name, value) {
      const n = String(name)
      const v = String(value)
      let found = false
      for (let i = 0; i < this._p.length;) {
        if (this._p[i][0] !== n) {
          i++; continue
        }
        if (!found) {
          this._p[i][1] = v
          found = true
          i++
        } else {
          this._p.splice(i, 1)
        }
      }
      if (!found)
        this._p.push([n, v])
      this._sync()
    }

    sort() {
      this._p.sort((a, b) => (a[0] < b[0] ? -1 : a[0] > b[0] ? 1 : 0)); this._sync()
    }

    forEach(cb, thisArg) {
      for (const [k, v] of this._p) cb.call(thisArg, v, k, this)
    }

    get size() {
      return this._p.length
    }

    * entries() {
      for (const [k, v] of this._p) yield [k, v]
    }

    * keys() {
      for (const [k] of this._p) yield k
    }

    * values() {
      for (const [, v] of this._p) yield v
    }

    [Symbol.iterator]() {
      return this.entries()
    }

    toString() {
      return this._p
        .map(([k, v]) => `${encodeQueryComponent(k)}=${encodeQueryComponent(v)}`)
        .join('&')
    }
  }

  function decodeQuery(s) {
    const plussed = s.replace(/\+/g, ' ')
    try {
      return decodeURIComponent(plussed)
    } catch {
      return plussed
    }
  }

  // ── URL ────────────────────────────────────────────────────────────────────

  // Parsing and every component mutation happen natively in ada (url.rs);
  // this class only owns the object surface. `_c` caches the component
  // strings in the order produced by url.rs — the U_* indices below are that
  // contract. A setter re-parses from `href`, applies the change natively and
  // swaps in the fresh array, so instances hold no native state.

  const U_HREF = 0
  const U_PROTOCOL = 1
  const U_USERNAME = 2
  const U_PASSWORD = 3
  const U_HOST = 4
  const U_HOSTNAME = 5
  const U_PORT = 6
  const U_PATHNAME = 7
  const U_SEARCH = 8
  const U_HASH = 9
  const U_ORIGIN = 10

  class URL {
    constructor(input, base) {
      const str = String(input)
      const c = urlParse(str, base === undefined || base === null ? undefined : String(base))
      if (c === null)
        throw new TypeError(`Invalid URL: "${str}"`)
      def(this, '_c', c)
      def(this, '_sp', null)
    }

    static parse(input, base) {
      try {
        return new URL(input, base)
      } catch {
        return null
      }
    }

    static canParse(input, base) {
      return URL.parse(input, base) !== null
    }

    // Apply one DOM setter natively. Everything except the href setter is a
    // silent no-op on invalid input (per spec), in which case url.rs returns
    // the components unchanged; `null` only comes back for an invalid href.
    _apply(which, value) {
      const c = urlSet(this._c[U_HREF], which, String(value))
      if (c !== null)
        this._c = c
      return c
    }

    // The spec resets a live URLSearchParams list whenever the query is set
    // through the URL side (href or search setter).
    _resyncSearchParams() {
      if (this._sp) {
        this._sp._p.length = 0
        this._sp._parse(this._c[U_SEARCH])
      }
    }

    _onSearchChanged() {
      this._apply(U_SEARCH, this._sp.toString())
    }

    get href() {
      return this._c[U_HREF]
    }

    set href(v) {
      if (this._apply(U_HREF, v) === null)
        throw new TypeError(`Invalid URL: "${String(v)}"`)
      this._resyncSearchParams()
    }

    get protocol() {
      return this._c[U_PROTOCOL]
    }

    set protocol(v) {
      this._apply(U_PROTOCOL, v)
    }

    get username() {
      return this._c[U_USERNAME]
    }

    set username(v) {
      this._apply(U_USERNAME, v)
    }

    get password() {
      return this._c[U_PASSWORD]
    }

    set password(v) {
      this._apply(U_PASSWORD, v)
    }

    get host() {
      return this._c[U_HOST]
    }

    set host(v) {
      this._apply(U_HOST, v)
    }

    get hostname() {
      return this._c[U_HOSTNAME]
    }

    set hostname(v) {
      this._apply(U_HOSTNAME, v)
    }

    get port() {
      return this._c[U_PORT]
    }

    set port(v) {
      this._apply(U_PORT, v)
    }

    get pathname() {
      return this._c[U_PATHNAME]
    }

    set pathname(v) {
      this._apply(U_PATHNAME, v)
    }

    get search() {
      return this._c[U_SEARCH]
    }

    set search(v) {
      this._apply(U_SEARCH, v)
      this._resyncSearchParams()
    }

    get hash() {
      return this._c[U_HASH]
    }

    set hash(v) {
      this._apply(U_HASH, v)
    }

    get origin() {
      return this._c[U_ORIGIN]
    }

    get searchParams() {
      if (!this._sp) {
        const sp = new URLSearchParams(this._c[U_SEARCH])
        def(sp, '_url', this)
        def(this, '_sp', sp)
      }
      return this._sp
    }

    toString() {
      return this._c[U_HREF]
    }

    toJSON() {
      return this._c[U_HREF]
    }
  }

  // ── Headers ────────────────────────────────────────────────────────────────

  const TOKEN_RE = /^[!#$%&'*+\-.^_`|~0-9A-Za-z]+$/

  function checkName(name) {
    const n = String(name)
    if (!TOKEN_RE.test(n))
      throw new TypeError(`Invalid header name: "${n}"`)
    return n.toLowerCase()
  }

  // Header names and values are ByteStrings on the wire and in Node's Headers.
  // Rejecting non-Latin-1 here rather than at the boundary means the throw
  // points at the user's line instead of failing host-side materialization.
  // eslint-disable-next-line no-control-regex -- the byte range is the point
  const BYTE_STRING_RE = /^[\x00-\xFF]*$/

  function checkValue(value) {
    const v = String(value).replace(/^[\t ]+|[\t ]+$/g, '')
    if (/[\0\r\n]/.test(v))
      throw new TypeError('Invalid header value')
    if (!BYTE_STRING_RE.test(v))
      throw new TypeError(`Invalid header value: must be a ByteString, got "${v}"`)
    return v
  }

  class Headers extends HeadersShell {
    constructor(init) {
      super()
      // Flat [name, value, name, value, …] — see webtypes.rs for the contract.
      def(this, '_l', [])
      if (init === undefined || init === null)
        return
      if (init instanceof Headers) {
        this._l.push(...init._l)
      } else if (Array.isArray(init)) {
        for (const pair of init) {
          if (!pair || pair.length !== 2)
            throw new TypeError('Headers: each entry must have two elements')
          this.append(pair[0], pair[1])
        }
      } else if (typeof init === 'object') {
        for (const k of Object.keys(init)) this.append(k, init[k])
      } else {
        throw new TypeError('Headers: invalid initializer')
      }
    }

    append(name, value) {
      this._l.push(checkName(name), checkValue(value))
    }

    delete(name) {
      const n = checkName(name)
      for (let i = this._l.length - 2; i >= 0; i -= 2) {
        if (this._l[i] === n)
          this._l.splice(i, 2)
      }
    }

    get(name) {
      const n = checkName(name)
      let out = null
      for (let i = 0; i < this._l.length; i += 2) {
        if (this._l[i] !== n)
          continue
        out = out === null ? this._l[i + 1] : `${out}, ${this._l[i + 1]}`
      }
      return out
    }

    getSetCookie() {
      const out = []
      for (let i = 0; i < this._l.length; i += 2) {
        if (this._l[i] === 'set-cookie')
          out.push(this._l[i + 1])
      }
      return out
    }

    has(name) {
      const n = checkName(name)
      for (let i = 0; i < this._l.length; i += 2) {
        if (this._l[i] === n)
          return true
      }
      return false
    }

    // Replaces the FIRST match in place; walking backwards kept the last one
    // and changed wire order.
    set(name, value) {
      const n = checkName(name)
      const v = checkValue(value)
      let found = false
      for (let i = 0; i < this._l.length;) {
        if (this._l[i] !== n) {
          i += 2; continue
        }
        if (!found) {
          this._l[i + 1] = v
          found = true
          i += 2
        } else {
          this._l.splice(i, 2)
        }
      }
      if (!found)
        this._l.push(n, v)
    }

    // Spec: iteration is sorted by name, with same-name values combined —
    // except set-cookie, which yields one entry per value.
    _sorted() {
      const names = []
      const seen = new Set()
      for (let i = 0; i < this._l.length; i += 2) {
        const n = this._l[i]
        if (!seen.has(n)) {
          seen.add(n); names.push(n)
        }
      }
      names.sort()
      const out = []
      for (const n of names) {
        if (n === 'set-cookie') {
          for (const v of this.getSetCookie()) out.push([n, v])
        } else {
          out.push([n, this.get(n)])
        }
      }
      return out
    }

    forEach(cb, thisArg) {
      for (const [k, v] of this._sorted()) cb.call(thisArg, v, k, this)
    }

    * entries() {
      for (const e of this._sorted()) yield e
    }

    * keys() {
      for (const [k] of this._sorted()) yield k
    }

    * values() {
      for (const [, v] of this._sorted()) yield v
    }

    [Symbol.iterator]() {
      return this.entries()
    }
  }

  // ── Body mixin ─────────────────────────────────────────────────────────────
  //
  // `_b` is null, a Uint8Array, or a string. Strings are kept as strings so a
  // text body survives the boundary without an encode/decode round trip.

  function bodyInit(body, headers) {
    if (body === undefined || body === null)
      return null
    if (typeof body === 'string') {
      if (!headers.has('content-type'))
        headers.set('content-type', 'text/plain;charset=UTF-8')
      return body
    }
    if (body instanceof URLSearchParams) {
      if (!headers.has('content-type'))
        headers.set('content-type', 'application/x-www-form-urlencoded;charset=UTF-8')
      return body.toString()
    }
    // Copied: an aliased buffer would let a later mutation of the caller's array
    // change an already-constructed Request/Response, and clone() share state.
    if (body instanceof Uint8Array)
      return new Uint8Array(body)
    if (ArrayBuffer.isView(body))
      return new Uint8Array(body.buffer.slice(body.byteOffset, body.byteOffset + body.byteLength))
    if (body instanceof ArrayBuffer)
      return new Uint8Array(body.slice(0))
    if (typeof body === 'object' && typeof body.getReader === 'function') {
      throw new TypeError(
        'A ReadableStream body cannot cross the sandbox boundary — buffer it first',
      )
    }
    return String(body)
  }

  function installBody(proto) {
    def(proto, 'arrayBuffer', async function arrayBuffer() {
      const b = consume(this)
      if (b === null)
        return new ArrayBuffer(0)
      const bytes = typeof b === 'string' ? utf8Encode(b) : b
      return bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength)
    })
    def(proto, 'bytes', async function bytes() {
      const b = consume(this)
      if (b === null)
        return new Uint8Array(0)
      return typeof b === 'string' ? utf8Encode(b) : b
    })
    def(proto, 'text', async function text() {
      const b = consume(this)
      if (b === null)
        return ''
      return typeof b === 'string' ? b : utf8Decode(b)
    })
    def(proto, 'json', async function json() {
      const b = consume(this)
      if (b === null)
        throw new SyntaxError('Unexpected end of JSON input')
      return JSON.parse(typeof b === 'string' ? b : utf8Decode(b))
    })
    Object.defineProperty(proto, 'bodyUsed', {
      get() {
        return this._used === true
      },
      enumerable: false,
      configurable: true,
    })
    // `.body` is deliberately absent rather than null: a ReadableStream cannot
    // cross the boundary, and returning null would claim the body is empty.
  }

  function consume(self) {
    if (self._used)
      throw new TypeError('Body has already been consumed')
    def(self, '_used', true)
    // `_b` is cleared as well as flagged, so a consumed body cannot be
    // resurrected by serializing the instance out (the codec reads `_b`).
    const body = self._b
    def(self, '_b', null)
    return body
  }

  // ── Request ────────────────────────────────────────────────────────────────

  const FORBIDDEN_BODY_METHODS = new Set(['GET', 'HEAD'])
  const FORBIDDEN_METHODS = new Set(['CONNECT', 'TRACE', 'TRACK'])

  class Request extends RequestShell {
    constructor(input, init = {}) {
      super()
      let url
      let method = 'GET'
      let headersInit
      let body = null

      if (input instanceof Request) {
        url = input._u
        method = input._m
        headersInit = input._h
        body = input._b
      } else {
        url = new URL(String(input)).href
      }

      if (init.method !== undefined) {
        const m = String(init.method)
        if (!TOKEN_RE.test(m))
          throw new TypeError(`Invalid method: "${m}"`)
        method = m.toUpperCase()
        // Forbidden by the fetch spec and rejected by Node's Request, so
        // rejecting here keeps the error on the user's line.
        if (FORBIDDEN_METHODS.has(method))
          throw new TypeError(`'${method}' HTTP method is unsupported`)
      }

      const headers = new Headers(init.headers !== undefined ? init.headers : headersInit)

      if (init.body !== undefined && init.body !== null) {
        if (FORBIDDEN_BODY_METHODS.has(method))
          throw new TypeError(`Request with ${method} method cannot have a body`)
        body = bodyInit(init.body, headers)
      } else if (body !== null && FORBIDDEN_BODY_METHODS.has(method)) {
        body = null
      }

      def(this, '_u', url)
      def(this, '_m', method)
      def(this, '_h', headers)
      def(this, '_b', body)
      def(this, '_used', false)
      def(this, '_x', init.redirect !== undefined ? String(init.redirect) : undefined)
    }

    get url() {
      return this._u
    }

    get method() {
      return this._m
    }

    get headers() {
      return this._h
    }

    get redirect() {
      return this._x === undefined ? 'follow' : this._x
    }

    clone() {
      if (this._used)
        throw new TypeError('Body has already been consumed')
      const copy = new Request(this._u, { method: this._m, headers: this._h })
      def(copy, '_b', this._b)
      def(copy, '_x', this._x)
      return copy
    }
  }
  installBody(Request.prototype)

  // ── Response ───────────────────────────────────────────────────────────────

  const NULL_BODY_STATUS = new Set([101, 103, 204, 205, 304])

  class Response extends ResponseShell {
    constructor(body = null, init = {}) {
      super()
      const status = init.status === undefined ? 200 : Number(init.status)
      // Exactly the spec range, which is also what Node accepts. An earlier
      // version carved out 101 for WebSocket upgrades; that is a workerd
      // behaviour we do not support, and it made the host constructor throw.
      // `Response.error()` is the only route to status 0.
      if (!Number.isInteger(status) || status < 200 || status > 599)
        throw new RangeError(`Response status ${status} is outside the range [200, 599]`)
      if (body !== null && body !== undefined && NULL_BODY_STATUS.has(status))
        throw new TypeError(`Response with status ${status} cannot have a body`)

      const statusText = init.statusText === undefined ? '' : String(init.statusText)
      if (/[\0\r\n]/.test(statusText) || !BYTE_STRING_RE.test(statusText))
        throw new TypeError(`Invalid statusText: "${statusText}"`)

      const headers = new Headers(init.headers)
      def(this, '_s', status)
      def(this, '_t', statusText)
      def(this, '_h', headers)
      def(this, '_b', bodyInit(body, headers))
      def(this, '_used', false)
    }

    get status() {
      return this._s
    }

    get statusText() {
      return this._t
    }

    get headers() {
      return this._h
    }

    get ok() {
      return this._s >= 200 && this._s < 300
    }

    get redirected() {
      return false
    }

    get type() {
      return this._s === 0 ? 'error' : 'default'
    }

    get url() {
      return ''
    }

    clone() {
      if (this._used)
        throw new TypeError('Body has already been consumed')
      const copy = new Response(null, {
        status: this._s === 0 ? 200 : this._s,
        statusText: this._t,
        headers: this._h,
      })
      def(copy, '_s', this._s)
      def(copy, '_b', this._b)
      return copy
    }

    static json(data, init = {}) {
      const body = JSON.stringify(data)
      if (body === undefined)
        throw new TypeError('Value is not JSON serializable')
      const headers = new Headers(init.headers)
      if (!headers.has('content-type'))
        headers.set('content-type', 'application/json')
      return new Response(body, { ...init, headers })
    }

    static error() {
      const r = new Response(null, { status: 200 })
      def(r, '_s', 0)
      return r
    }

    static redirect(url, status = 302) {
      if (![301, 302, 303, 307, 308].includes(status))
        throw new RangeError(`Invalid redirect status: ${status}`)
      return new Response(null, { status, headers: { location: new URL(String(url)).href } })
    }
  }
  installBody(Response.prototype)

  // ── Publish ────────────────────────────────────────────────────────────────

  for (const [name, value] of [
    ['Headers', Headers],
    ['Request', Request],
    ['Response', Response],
    ['TextEncoder', TextEncoder],
    ['TextDecoder', TextDecoder],
    ['URL', URL],
    ['URLSearchParams', URLSearchParams],
  ]) {
    Object.defineProperty(globalThis, name, {
      value,
      writable: true,
      enumerable: false,
      configurable: true,
    })
  }
})
