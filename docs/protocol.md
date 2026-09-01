# iso4 Wire Protocol

Communication between the TypeScript host (`@iso4/sandbox`) and the Rust V8
binary (`iso4-v8`) happens over a Unix domain socket using length-prefixed
binary frames. The frame envelope is small and stable; **every JavaScript
value** inside a payload travels as a V8 serialization blob (§4).

Two planes, deliberately separated:

- **Control plane** — callIds, export names, specifiers, prefix ids, limits,
  error codes/names/messages. Plain integers and length-prefixed strings, so
  either side can read a frame's routing without touching V8.
- **Data plane** — every JS value. One `ValueBlob` slot per crossing, produced
  by V8's own serializer on the writing side and read by V8's own
  deserializer on the other. No hand-written value codec sits between the two
  V8s.

The direction a frame travels is known from context, so message type bytes are
scoped per direction. A `0x01` frame from TS means `Authenticate`; a `0x01`
frame from Rust means `BridgeCall`.

---

## 1. Frame envelope

Every message, in both directions, uses the same outer envelope:

```txt
┌─────────────────────┬──────────────────┬─────────────────────────┐
│  length  (4 bytes)  │  type  (1 byte)  │  payload  (N bytes)     │
│  uint32 big-endian  │  see tables      │  message-specific       │
└─────────────────────┴──────────────────┴─────────────────────────┘
```

| Field     |               Size | Encoding         | Meaning                                                                  |
| --------- | -----------------: | ---------------- | ------------------------------------------------------------------------ |
| `length`  |            4 bytes | `u32` big-endian | Byte count of `type + payload`; an empty payload frame has `length = 1`. |
| `type`    |             1 byte | `u8`             | Message type from the direction-specific table.                          |
| `payload` | `length - 1` bytes | message-specific | Encoded as described in §3–§6.                                           |

Frame readers MUST reject:

| Condition                                      | Error            |
| ---------------------------------------------- | ---------------- |
| `length == 0`                                  | invalid frame    |
| `length > maxFrameLength`                      | invalid frame    |
| EOF before all `length` bytes arrive           | connection error |
| unknown message type for the current direction | protocol error   |

Current protocol version: **`1`** — nothing is released yet, so there is no
version history and no compatibility handling: both sides must speak exactly
this version, and the handshake hard-fails otherwise (§8).

---

## 2. Message tables

### 2.1 TS → Rust

|   Byte | Name             | Payload                 | Response                                                                                                                                  |
| -----: | ---------------- | ----------------------- | ----------------------------------------------------------------------------------------------------------------------------------------- |
| `0x01` | `Authenticate`   | `AuthenticatePayload`   | exactly one `Hello`; on a malformed payload Rust closes the socket without replying                                                       |
| `0x02` | `Run`            | `RunPayload`            | zero or more `BridgeCall`, then exactly one `Result`; when it reports pending background work: more `BridgeCall`s, then one `RunComplete` |
| `0x03` | `Precompile`     | `PrecompilePayload`     | exactly one `PrecompileResult`                                                                                                            |
| `0x04` | `PrefixRun`      | `PrefixRunPayload`      | same as `Run`                                                                                                                             |
| `0x05` | `DisposePrefix`  | `PrefixId`              | no frame; idempotent                                                                                                                      |
| `0x06` | `BridgeResponse` | `BridgeResponsePayload` | resumes the waiting sandbox bridge call                                                                                                   |
| `0x07` | `Terminate`      | `RunId`                 | Rust sends one `Result` with `ERR_ABORTED` (graceful abort); a CPU-bound run not reading frames is instead reclaimed by teardown          |
| `0x08` | `Stats`          | empty                   | exactly one `StatsResult`                                                                                                                 |
| `0x09` | `StreamChunk`    | `StreamChunkPayload`    | one chunk of a streamed body (§5.5), inside the granted credit window; no reply                                                           |
| `0x0A` | `StreamEnd`      | `StreamEndPayload`      | end of a streamed body: clean EOF or a source failure; no reply                                                                           |

### 2.2 Rust → TS

|   Byte | Name               | Payload                   | Notes                                                                                                 |
| -----: | ------------------ | ------------------------- | ----------------------------------------------------------------------------------------------------- |
| `0x01` | `BridgeCall`       | `BridgeCallPayload`       | Sandbox called a configured host global/function or host import.                                      |
| `0x02` | `Result`           | `RunCompletionPayload`    | Final completion for `Run` or `PrefixRun`; sent exactly once. Includes captured stdout/stderr arrays. |
| `0x03` | `PrecompileResult` | `PrecompileResultPayload` | Result of `Precompile`.                                                                               |
| `0x04` | `Log`              | `DiagnosticLogPayload`    | Internal runtime diagnostic; not sandbox stdout/stderr.                                               |
| `0x05` | `Hello`            | `HelloPayload`            | Handshake acknowledgement; the first frame the runtime sends, answering `Authenticate`.               |
| `0x06` | `StatsResult`      | `StatsPayload`            | Capacity/usage snapshot answering a `Stats` request.                                                  |
| `0x07` | `RunComplete`      | `RunCompletePayload`      | Final frame of a run whose `Result` reported pending background work; frees the run's slot.           |
| `0x08` | `StreamPull`       | `StreamPullPayload`       | Credit grant: the sandbox consumed streamed-body bytes; the host may send that many more (§5.5).      |
| `0x09` | `StreamCancel`     | `StreamCancelPayload`     | The sandbox cancelled a streamed body; the host stops pumping and releases the source (§5.5).         |

---

## 3. Primitive payload encodings

All integers are big-endian.

| Type          | Encoding                              |
| ------------- | ------------------------------------- |
| `u8`          | 1 byte                                |
| `u16`         | 2 bytes, big-endian                   |
| `u32`         | 4 bytes, big-endian                   |
| `u64`         | 8 bytes, big-endian                   |
| `f64`         | 8 bytes, IEEE-754 big-endian          |
| `bool`        | `u8`: `0 = false`, `1 = true`         |
| `String`      | `u32 byteLength` + UTF-8 bytes        |
| `Bytes`       | `u32 byteLength` + raw bytes          |
| `Optional<T>` | `u8 present`; if `1`, followed by `T` |
| `List<T>`     | `u32 length` + repeated `T`           |
| `ValueBlob`   | `u32 byteLength` + V8 blob (§4)       |

Strings MUST be valid UTF-8. Decoders MUST reject invalid booleans and invalid
optional presence bytes.

A `List<T>` length MUST be backed by the payload that carries it. Every entry
costs at least one byte on the wire, so a length greater than the number of
bytes remaining describes a payload that cannot exist and MUST be rejected as
malformed before a decoder sizes anything from it. A decoder that does size a
collection from a wire-supplied length MUST also be able to decline: a
reservation it cannot satisfy is a decode error for that connection, never a
process-level failure.

---

## 4. Value encoding

Every JavaScript value that crosses the boundary — exports, data globals,
host-module data leaves, bridge arguments, bridge return values, error
`fields` — is a **V8 serialization blob**: the byte format produced by V8's
`ValueSerializer` and consumed by V8's `ValueDeserializer`.

The slot is always the same shape, in both directions:

```txt
┌──────────────────────┬────────────────────────────┐
│  byteLength (u32 BE) │  V8 serialization blob     │
└──────────────────────┴────────────────────────────┘
```

There is **no tag byte**: there is exactly one value codec, so nothing needs
discriminating. Where a slot is optional it keeps the usual `Optional<T>`
presence byte in front (`u8 present`, then the slot).

| Direction | Slot                      | Frame                          | Blob Content                        |
| --------- | ------------------------- | ------------------------------ | ----------------------------------- |
| TS → Rust | data-global value         | `Run`/`Precompile`/`PrefixRun` | the value                           |
| TS → Rust | host-module data leaf     | `Run`/`Precompile`             | the value                           |
| TS → Rust | bridge resolve value      | `BridgeResponse` (ok = 1)      | the value                           |
| TS → Rust | bridge error `fields`     | `BridgeResponse` (ok = 0)      | the fields object                   |
| Rust → TS | bridge call `args`        | `BridgeCall`                   | **one blob = the whole args array** |
| Rust → TS | module exports            | `Result` (ok = 1)              | **one blob = one `{name: value}`**  |
| Rust → TS | run error `fields`        | `Result` (ok = 0)              | the fields object                   |
| Rust → TS | precompile error `fields` | `PrecompileResult`             | the fields object                   |

One blob per crossing, never one blob per value: serializing N values together
is measurably faster than N blobs, produces the same total bytes, and
preserves identity between values that reference the same object.

### 4.1 Implementation requirements

Both sides must set these up exactly; each detail is load-bearing.

**TypeScript host** (`src/v8-codec.ts`):

- Out: `new v8.DefaultSerializer()`, then
  `_setTreatArrayBufferViewsAsHostObjects(false)` — **mandatory**. Node's
  default `v8.serialize()` writes typed arrays with a Node-private
  host-object tag that a plain (non-Node) V8 rejects at read time. Then
  `writeHeader()`, `writeValue()`, `releaseBuffer()`.
- In: `v8.deserialize()` (handles byteOffset views correctly).

**Rust runtime** (`src/blob.rs`):

- Out: `v8::ValueSerializer` with `write_header()` **before** `write_value()`.
- In: `v8::ValueDeserializer` with `read_header()` **before** `read_value()`.
  Skipping `read_header` does not fail cleanly — the format-version byte is
  then read as a value tag and every payload dies with a misleading
  host-object error.
- Neither delegate claims host objects, so the bytes stay plain-V8 readable.

### 4.2 What crosses

The boundary carries **data, not behavior**. What V8's format can represent,
arrives as a real instance:

| Value                                             | Result                                        |
| ------------------------------------------------- | --------------------------------------------- |
| `undefined`, `null`, boolean, number, string      | as-is                                         |
| `bigint`                                          | as-is, arbitrary precision                    |
| `Date`, `Map`, `Set`, `RegExp`                    | real instance                                 |
| `Error` (and subclasses)                          | real instance; `message` and `name` survive   |
| `ArrayBuffer`, every `TypedArray`, `DataView`     | real instance, element type preserved         |
| a `subarray` window                               | only the window's bytes                       |
| plain objects and arrays, including sparse arrays | as-is                                         |
| cyclic and shared references                      | back-references; object identity is preserved |

What cannot be represented is rejected **loudly**, in both directions:

| Value                                       | Error                                                                                        |
| ------------------------------------------- | -------------------------------------------------------------------------------------------- |
| function                                    | `ERR_EXPORT_NOT_SERIALIZABLE` or `ERR_FUNCTION_ARGUMENT_NOT_SUPPORTED` depending on boundary |
| unresolved `Promise`                        | `ERR_EXPORT_NOT_SERIALIZABLE`                                                                |
| `Symbol`                                    | `ERR_EXPORT_NOT_SERIALIZABLE`                                                                |
| `WeakMap` / `WeakSet` / `Proxy`             | `ERR_EXPORT_NOT_SERIALIZABLE`                                                                |
| any of the above returned by a host handler | `ERR_HOST_BRIDGE`                                                                            |

A run whose exports contain a function or an unresolved promise reports the
offending export by name (`export "handler" is a function`); anything V8
refuses deeper in the graph reports V8's own data-clone message.

**Class instances flatten silently.** `new Tenant()` arrives as
`{ id: "t1" }` — its own enumerable properties, no prototype, no methods.
This is an accepted trade-off, not an oversight: Node's serializer exposes no
hook to reject class instances (workerd's `treatClassInstancesAsPlainObjects
= false` is a V8 patch that is not available here). Copy what you mean to
send into a plain object. Custom classes that must survive intact need a
per-type serializer, which is roadmap work, not a flag.

**`"__proto__"` as an own key passes through as a plain own key.** V8's
serializer writes it as an own data property and the deserializer _defines_
it (never `[[Set]]`s it), so the receiving object's prototype is untouched
and no prototype pollution is possible:

```js
// host handler returns:
Object.defineProperty({ x: 1 }, '__proto__', { value: { polluted: true }, enumerable: true })
// sandbox receives an object where:
//   Object.hasOwn(v, '__proto__') === true
//   Object.getPrototypeOf(v)      === Object.prototype
//   ({}).polluted                 === undefined
```

Objects are serialized with their own enumerable string-keyed properties;
prototype methods and non-enumerable properties are not carried.

### 4.3 Example

Sandbox code:

```js
export default { ok: true }
export const count = 2
```

The `exports` slot is **one** blob holding a single flat object — `default`
is not a separate field in the run result, it is just the property named
`"default"`:

```ts
const result = {
  ok: true,
  exports: {
    default: { ok: true },
    count: 2,
  },
  stdout: [],
  stderr: [],
  durationMs: 1,
}
```

Byte-level, the frame slot is `u32 byteLength` followed by the blob, whose
first two bytes are V8's header tag `0xFF` and the serialization **format
version** (the value the handshake in §5.1 agrees on).

### 4.4 Host types

Some classes cross as **real instances** rather than flattening the way §4.2
describes. They use V8's own host-object escape hatch — the same mechanism
workerd uses (`src/workerd/jsg/ser.h`).

A host object is written as V8's `kHostObject` tag (`0x5C`) where a value is
expected, followed by an embedder-defined payload: a type tag and then
whatever that type's codec writes. This section defines the framing; each
registered type defines its own body.

The set of types is **open**. Adding one means allocating a tag and
registering a codec pair on both sides; nothing else in the protocol changes.
The first family to be registered is the web types (`Headers`, `Request`,
`Response`).

#### 4.4.1 Type tags

One registry for the whole protocol. **Numeric values never change.** `0` is
reserved so that a stray zero is never a valid type.

| Tag | Type             | Status                                    |
| --- | ---------------- | ----------------------------------------- |
| 0   | _invalid_        | reserved; reading it is a protocol fault  |
| 1   | `Headers`        | implemented                               |
| 2   | `Request`        | implemented                               |
| 3   | `Response`       | implemented                               |
| 4   | `ReadableStream` | **reserved, not implemented** (see 4.4.5) |
| 5   | `WritableStream` | **reserved, not implemented**             |
| 6   | `WebSocket`      | **reserved, not implemented**             |
| 7   | `AbortSignal`    | **reserved, not implemented**             |
| 8   | `DOMException`   | **reserved, not implemented**             |
| 9   | `Blob`           | **reserved, not implemented**             |
| 10  | `FormData`       | **reserved, not implemented**             |
| 11  | `URLPattern`     | **reserved, not implemented**             |

Reading a tag that this build does not implement is
`ERR_TYPE_NOT_SERIALIZABLE`, not a protocol fault: it is the expected outcome
of a peer built with a wider type set, and the message names the tag.

#### 4.4.2 Payload primitives

Inside a host-object payload the integers are V8's own **varint** encoding
(`ValueSerializer::WriteUint32` / `ReadUint32`) — _not_ the big-endian frame
integers of §3. Three composites are used:

```txt
str   := u32 byteLength, UTF-8 bytes             (no NUL terminator)
value := a nested V8 value, written with WriteValue / read with ReadValue
blob  := u32 byteLength, V8 serialization blob   (byteLength 0 = absent)
```

`value` is the important one. Anything that is already a JavaScript object on
both sides — the header list, the body — is written with a nested `WriteValue`
rather than framed by hand. V8 then walks it internally instead of the embedder
pushing elements across the API boundary one at a time, which is the cost
measured at ~87 ns per `obj.set()` when the old `WireValue`
codec. Only scalars that are _not_ already JS values (a status code, a URL
string being read out of an instance) are hand-framed.

#### 4.4.3 Headers

Used by `Request` and `Response`, and as the whole payload of tag 1.

```txt
value   entries   // one flat array: [name, value, name, value, …]
```

Names are pre-lowercased. The array is flat rather than an array of pairs
because that is how the sandbox stores headers internally, so neither side has
to rebuild it.

No name is special-cased. An earlier draft interned ~40 common names to a
single varint the way workerd does (`api/headers.c++`), which saves roughly
100 bytes per response — but it requires a frozen lookup table duplicated in
both codecs, and a table that drifts by one position delivers headers under
the **wrong names** silently. Not worth that failure mode at this scale.

Duplicates are separate entries, so multiple `set-cookie` values survive
intact — a `Record<string, string>` cannot represent them and must not be used
as an intermediate anywhere on either side.

Readers **must** reject an array longer than 2048 elements (1024 entries), and
an odd-length array, before constructing anything.

#### 4.4.4 Request and Response

```txt
request (tag 2)
  str     url
  str     method       // uppercased; "GET" is written explicitly
  value   headers      // §4.4.3
  value   body         // null | string | Uint8Array
  blob    extras       // plain data only — see below

response (tag 3)
  u32     status
  str     statusText   // "" when unset
  value   headers
  value   body
  blob    extras
```

There is no body-kind discriminator: V8 records whether the value is `null`, a
string, or a typed array, so the reader gets the right type back without the
two sides agreeing on a tag byte. Writing the body as a nested value also means
V8 copies the bytes once, straight out of the backing store; hand-framing it
required a copy into an intermediate buffer and then a second copy into the
serializer.

A body that is anything else — a stream — is rejected before it reaches V8, so
the error names the real problem rather than surfacing as "could not be
cloned". This applies to the sandbox → host direction: a hydrated streamed
body (§5.5) cannot be sent back and is refused with a message naming the
remedy (read it first). Host → sandbox, a large body crosses as a **stream
handle** in the descriptor (§4.4.6) with the bytes following as `StreamChunk`
frames — see §5.5.

`extras` is a length-delimited V8 blob holding a plain object of
forward-compatible fields (`redirect`, `cf`, `signal`, …). It exists so new
fields can be added **without allocating a new type tag** — the pattern workerd
uses for the same reason (`Request::serialize` in `api/http.c++`). A zero
byteLength means no extras.

`extras` must contain only plain data. It must **not** contain a nested host
object: the host side hand-writes these payloads (§4.4.6) and Node cannot emit
a host object inside a `writeValue` graph.

#### 4.4.5 What does not cross

Every case below reports `ERR_TYPE_NOT_SERIALIZABLE` with a message naming
the type and the reason. There is one code for all of them, deliberately:
the type set is open, and a caller's handling of "this value cannot cross"
does not differ per type.

| Value                                                           | Reason                        |
| --------------------------------------------------------------- | ----------------------------- |
| a sandbox body that is a stream (outbound)                      | not self-contained            |
| `WebSocket`, `AbortSignal`                                      | not self-contained            |
| a tag this build does not implement                             | unimplemented type            |
| a host type nested below the top level of a host → sandbox slot | unreachable position (§4.4.6) |

Types that are **not self-contained** are the general category, borrowed from
workerd's `ExternalHandler` split (`jsg/ser.h:62`): a value that refers to a
resource elsewhere rather than carrying its content. Such a value can only be
serialized in a context that offers somewhere to put the reference. iso4 has
no such context today, so the answer is always the error; the seam exists so
one can be added without touching any tag.

Streams are the clearest instance, and deliberately unsupported rather than
buffered-behind-the-scenes.
workerd can serialize a `ReadableStream` only because its boundary sits on a
live capability-passing RPC connection: `ReadableStream::serialize` writes no
bytes at all, it mints a `capnp::ByteStream` capability, puts it in the
message's cap table via an `ExternalHandler`, and pumps the body over that
connection afterwards. A one-shot iso4 frame has no such channel, so a stream
has nowhere to go. Tags 4–6 are reserved so that adding one later is not a
format change — a streaming body would simply be one of those host objects
sitting in the body slot, which needs no new framing at all.

#### 4.4.6 Direction asymmetry

The two directions use different mechanisms. Both support nesting at any depth;
they get there differently.

| Leg                          | Mechanism                                     |
| ---------------------------- | --------------------------------------------- |
| Rust writes (sandbox → host) | V8 routes automatically on internal fields    |
| Node reads (sandbox → host)  | `v8.Deserializer` subclass, `_readHostObject` |
| Node writes (host → sandbox) | **branded plain objects** — see below         |
| Rust reads (host → sandbox)  | ordinary deserialize, then a rehydration walk |

The sandbox classes are backed by `FunctionTemplate` instances with internal
fields, so V8 routes them to `WriteHostObject` off a map field read, with
`HasCustomHostObject()` left `false`. No embedder callback fires for ordinary
objects. Enabling it would make V8 call back into the embedder for _every_ plain
object serialized.

The internal field is deliberately left **empty** (zeroed at construction).
The type tag lives in a **private-symbol property** stamped onto every
instance at construction. Private symbols are unreachable from guest JS, so
the tag cannot be forged or removed, and identifying an instance never
consults guest-mutable state (`globalThis` lookups, `instanceof`,
`Symbol.hasInstance`). The runtime likewise captures the three class
references once at install time into private slots, so rehydrating an
instance wires its prototype from a stable handle rather than the live
global.

Node has no write-side equivalent. `v8.Serializer` exposes no delegate to
JavaScript and `_writeHostObject` never fires for a class instance — the object
silently flattens. So the host does not attempt it: each instance is replaced
by a plain object carrying the same fields plus a **brand key** holding its
type tag, the graph is serialized normally, and the runtime walks the result
substituting real instances.

The brand key is `__iso4_ht_` followed by the sandbox's 16-byte descriptor
token (§5.1) in lowercase hex — 42 characters total. The runtime rehydrates a
descriptor **only** under that exact key: because the key contains the
session's random token, inbound structured data cannot name it, so a
descriptor-shaped object in untrusted data (including one carrying the
well-known `__iso4_ht_` prefix) passes through as the plain data it is. No
property name is reserved.

```txt
// brand key:        __iso4_ht_<32 hex chars of the session token>
// host has:         { meta: 'x', res: Response }
// host serializes:  { meta: 'x', res: { [brandKey]: 3, status: 200,
//                                       statusText: '', headers: [...],
//                                       body: Uint8Array } }
```

A descriptor whose body outgrew the host's probe carries a `bodyStream`
field (a `u32` stream id) instead of inline body bytes; the runtime hydrates
it into a socket-backed stream the sandbox reads through `.body` and the body
helpers, with the bytes following as `StreamChunk` frames (§5.5). Only
stamped descriptors are interpreted at all, so untrusted data cannot name a
stream handle.

Because the graph is an ordinary V8 value, nesting, cycles, `Map`/`Set` and
object identity all work for free, and a new type costs one tag plus one
constructor in the rehydration switch. The walk runs only on host → sandbox
legs — bridge responses, data globals, and call args — and is guarded by a
byte scan for the session brand key, so a payload with no stamped descriptors
pays only that scan (the random token in the needle makes false positives
practically impossible). Depth is capped at 32 levels on both sides.

The asymmetry is safe because the two directions never share a reader: Rust reads
what Node writes and vice versa, never both.

Sandbox-side subclasses (`class My extends Response {}`) keep their internal
fields, receive the type stamp in the shell constructor `super()` reaches, and
route normally. A _lookalike_ that re-points its prototype without calling the
real constructor has no internal field and no stamp: it crosses as the plain
data it is.

#### 4.4.7 Versioning

Following workerd's rule (`jsg/ser.h`): the byte sequence for a given tag is
frozen once anything has written it. A changed layout means a **new tag**,
with the reader for it deployed everywhere before any writer starts emitting
it. Additive fields go in `extras` and need no tag at all.

---

## 5. Message payload schemas

### 5.1 Handshake

`AuthenticatePayload` (TS → Rust, MUST be the first frame on every connection):

| Field             | Encoding                 | Notes                                             |
| ----------------- | ------------------------ | ------------------------------------------------- |
| `protocolVersion` | `u16`                    | Must equal the runtime's `PROTOCOL_VERSION`.      |
| `probe`           | `u32 byteLength` + bytes | A serialized `null` — see below.                  |
| `descriptorToken` | `u32 byteLength` + bytes | Exactly **16 random bytes** — see below (§4.4.6). |

The payload ends with the token; a wrong-size token and trailing bytes are
rejected as malformed. There is no auth token — access to the socket is
controlled by the owner-only (0700) per-sandbox directory the host creates it
in, which the kernel checks on every `connect(2)`.

**The descriptor token** is not an authentication secret; it authenticates
**data**, not peers. The host draws 16 random bytes once per sandbox and sends
the same token on every connection. Host-emitted host-type descriptors
(§4.4.6) are stamped with the brand key derived from it, and the runtime
rehydrates only descriptors carrying that key — so inbound structured data
that merely looks like a descriptor is never reinterpreted as a host type.
The token never reaches sandbox code: descriptors are replaced wholesale, key
and all, before a value is handed to guest JS.

`HelloPayload` (Rust → TS, the first frame the runtime sends):

| Field     | Encoding                 | Notes                                                                |
| --------- | ------------------------ | -------------------------------------------------------------------- |
| `status`  | `u8`                     | `0 = ok`, `1 = protocol version mismatch`, `2 = V8 format mismatch`. |
| `probe`   | `u32 byteLength` + bytes | The runtime's own serialized `null`.                                 |
| `message` | `String`                 | Actionable detail for a non-zero status; empty when `status = 0`.    |

**The `Authenticate` frame is read on its own terms.** It arrives from a peer
that has shown nothing yet, so it does not get the frame ceiling the rest of
the connection uses (§2): the runtime caps it at **4 KiB**, which is ample for
a `u16` and a short probe, and it MUST arrive complete within **2
seconds** of the connection being accepted. The deadline covers the frame as a
whole rather than each read within it, so a peer that sends its bytes slowly is
bounded by the same budget as one that sends nothing. Both are enforced only
before authentication; once the handshake is accepted, reads have no deadline,
because a pooled connection legitimately sits idle between runs. A peer that
misses either is dropped without a reply.

**Why the probe.** Values cross as V8 serialization blobs, so both V8s must
agree on the serialization **format version**. V8 bumps that version over
time and `ReadHeader` hard-rejects anything newer than the reader knows;
neither Node nor rusty_v8 exposes a way to pin what they write. The probe is
a serialized `null`: byte 0 is the header tag `0xFF` and byte 1 is the
writer's format version.

**The check is startup-only and hard-fails.** There is no per-frame
negotiation and no fallback codec:

1. The runtime computes its own probe once at process start, in a throwaway
   isolate. At handshake time the check is a byte comparison — no isolate
   plumbing reaches the session layer, and no per-connection V8 work happens.
2. On a protocol-version or format-version mismatch the runtime sends a
   `Hello` carrying the error status and an actionable message, then closes.
   (On a **malformed payload** it closes silently — that peer is not speaking
   this protocol at all.)
3. The host awaits the `Hello`, and empirically `v8.deserialize`s the
   runtime's probe rather than trusting the version byte alone. Any failure
   rejects `createSandbox()` with a typed error naming the remedy: update
   `@iso4/sandbox` and `@iso4/v8-*` together (§8).

Cost: one small frame each way, once per connection at `createSandbox()`
time. Never per run, never per value.

### 5.2 Run payloads

`RunPayload`:

| Field      | Encoding              | Notes                                                                                                       |
| ---------- | --------------------- | ----------------------------------------------------------------------------------------------------------- |
| `runId`    | `u32`                 | Unique on this connection; wraps unsigned at 2³². Echoed back on the `Result` and checked there — see §5.7. |
| `code`     | `String`              | ESM source.                                                                                                 |
| `filename` | `Optional<String>`    | Used in stack traces.                                                                                       |
| `limits`   | `ResourceLimits`      | Only caller-set fields sent; runtime fills defaults.                                                        |
| `globals`  | `List<GlobalDef>`     | Host globals + how the runtime installs each one.                                                           |
| `imports`  | `List<ImportBinding>` | Source or host import declarations.                                                                         |
| `call`     | `Optional<CallSpec>`  | Host → sandbox call resolved against the freshly evaluated module.                                          |

`PrefixRunPayload`:

| Field      | Encoding             | Notes                                                                                                                      |
| ---------- | -------------------- | -------------------------------------------------------------------------------------------------------------------------- |
| `runId`    | `u32`                | Unique on this connection; wraps unsigned at 2³². Echoed back on the `Result` and checked there — see §5.7.                |
| `prefixId` | `PrefixId`           | Prefix handle returned by `Precompile`.                                                                                    |
| `code`     | `Optional<String>`   | ESM postfix source. A frame carries a postfix **or** a call — exactly one; both parser and encoder enforce this.           |
| `filename` | `Optional<String>`   | Used in stack traces.                                                                                                      |
| `limits`   | `ResourceLimits`     | Fully normalized by TS before sending.                                                                                     |
| `globals`  | `List<GlobalDef>`    | Bridge stubs to re-install; subset of predeclared. Always `bridge` kind (values are replayed from the stored prefix defs). |
| `imports`  | `List<ImportRebind>` | Locations of host-import function leaves whose handler was replaced for this run.                                          |
| `call`     | `Optional<CallSpec>` | Host → sandbox call resolved against the prefix module's exports.                                                          |

`CallSpec`:

A host → sandbox function call. `exportPath` addresses a callable **relative
to the module's exports** — a top-level exported function (`"handler"`) or a
method on an exported object (`"default.fetch"`) — never `globalThis`. The
receiver is the object the final path segment was read from (plain `a.b.c()`
semantics; the namespace itself for a single segment), and property reads
follow the prototype chain so `export default new Worker()` resolves
prototype methods. Resolution happens after the module settles, inside the
run's CPU budget (a segment may be an accessor). A path that does not resolve
— or resolves to something not callable — fails the run with
`ERR_CALL_TARGET_NOT_FOUND`. Segment count is capped at 16.

When `call` is present the run's **result value is the called function's
return value** (awaited first when it is a Promise) instead of the exports —
never both. The completion payload is unchanged: it carries exactly one value
blob either way, and the host knows which it asked for. `maxExportBytes`
applies to the value blob; a sync return value completes inside the run's
start turn, while an async one suspends on the same turn machinery as the
module promise (bridge calls included).

| Field        | Encoding    | Notes                                                                                                                                               |
| ------------ | ----------- | --------------------------------------------------------------------------------------------------------------------------------------------------- |
| `exportPath` | `String`    | Dot-separated path relative to the module's exports.                                                                                                |
| `argsBlob`   | `ValueBlob` | **One** blob holding the whole argument array — same convention as `BridgeCall` args (§5.4); host types (`Request`, …) rehydrate to real instances. |

Call args are host-authored (the trusted direction), so no dedicated size
limit applies — like bridge responses, the frame read is capped by `memoryMb`.

`PrecompilePayload`:

| Field      | Encoding              | Notes                                                          |
| ---------- | --------------------- | -------------------------------------------------------------- |
| `code`     | `String`              | ESM prefix source.                                             |
| `filename` | `Optional<String>`    | Used in stack traces.                                          |
| `limits`   | `ResourceLimits`      | Currently unused at precompile (validation is unlimited).      |
| `globals`  | `List<GlobalDef>`     | Declares the global shape; stored and replayed into every run. |
| `imports`  | `List<ImportBinding>` | Stored with the prefix; host imports declare bridge shape.     |

`ResourceLimits`:

Every field is `Optional<u32>`. The client sends only the limits the caller
explicitly set; the runtime fills any absent field from its own defaults (it
owns the default safety posture — the numbers below live in
`native/v8-runtime/src/ipc.rs` as the source of truth). An **absent** field
means "apply the runtime default"; an **explicit `0`** means "no limit" and is
distinct from absent.

| Field                | Encoding        | Default  | Notes                                                                                                                                                            |
| -------------------- | --------------- | -------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `memoryMb`           | `Optional<u32>` | `64`     | Zero = no limit.                                                                                                                                                 |
| `cpuTimeMs`          | `Optional<u32>` | `5000`   | Zero = no limit.                                                                                                                                                 |
| `wallTimeMs`         | `Optional<u32>` | `30000`  | Zero = no limit.                                                                                                                                                 |
| `maxExportBytes`     | `Optional<u32>` | `16 MiB` | Max byte length of the exports value blob. Zero = no limit. Violation → `ERR_EXPORT_TOO_LARGE`.                                                                  |
| `maxStdoutBytes`     | `Optional<u32>` | `1 MiB`  | Max bytes captured across all stdout lines. Zero = no limit. Lines that would exceed the cap are silently dropped.                                               |
| `maxStderrBytes`     | `Optional<u32>` | `1 MiB`  | Max bytes captured across all stderr lines. Zero = no limit. Lines that would exceed the cap are silently dropped.                                               |
| `maxBridgeCallBytes` | `Optional<u32>` | `16 MiB` | Max byte length of a single `BridgeCallPayload` (sandbox → host args). Zero = no limit (64 MiB framing cap applies). Violation → `ERR_BRIDGE_PAYLOAD_TOO_LARGE`. |
| `maxBridgeCalls`     | `Optional<u32>` | `10`     | Maximum total bridge calls (globals + host imports combined) a single run may make. Zero = no limit. Violation → `ERR_BRIDGE_CALL_LIMIT_EXCEEDED`.               |
| `graceMs`            | `Optional<u32>` | `30000`  | Wall budget for `waitUntil` background work after the Result ships, one budget for the whole registered set. Zero disables the grace phase entirely.             |

`GlobalDef`:

Each host global is installed **natively** by the runtime — the client never
prepends generated source to user code, so user code always starts at line 1,
and a global's name reaches the sandbox global object through the V8 API
(`object.set`), never interpolated into an identifier position. Every entry is
a `u8` kind tag, then a `String` name, then a `bool enumerable`, then a
kind-specific tail:

| Kind Byte | Kind     | Tail                              | Install                                                                                                                            |
| --------- | -------- | --------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------- |
| `0x00`    | `bridge` | —                                 | Bridge stub under `name` (issues `BridgeCall` frames).                                                                             |
| `0x01`    | `string` | `String expr`                     | Runtime evaluates `(expr)` as its own script; sets `globalThis[name]`.                                                             |
| `0x02`    | `data`   | `ValueBlob value`                 | Materialised via the value codec (§4); sets `globalThis[name]`.                                                                    |
| `0x03`    | `shim`   | `String shim, String handlerName` | Installs a bridge stub under `handlerName` and a wrapper `async (...a) => shim(await globalThis[handlerName](...a))` under `name`. |

`enumerable` is the host's per-global opt-out of enumeration (`false` installs
the public name with `DONT_ENUM`, keeping it out of `for...in` /
`Object.keys` while staying reachable by name). It applies to the name a run
sees; the runtime installs the same attribute on that name's between-runs
placeholder so the enumerated surface never differs between prepare time and
run time. Runtime-internal names (`__iso4_*`, e.g. a shim's `handlerName`)
install non-enumerable regardless of the flag.

Only `bridge` and `shim` install a bridge stub (and so require the session
socket); `string`/`data` are pure in-isolate installs. On `PrefixRun` every
entry is `bridge` kind — string/data globals and shim wrappers were declared
at `Precompile` time and are replayed from the stored prefix defs when the
prefix evaluates, so only their bridge stubs are re-installed per run.

`ImportBinding` (`Run` / `Precompile`):

Both public import flavors cross the wire structurally — the client never
generates sandbox source. A source module carries ESM text verbatim; a host
module carries its **shape** as a tree of data. The runtime builds host
modules natively (see DESIGN.md §4.3): data leaves are materialised from their
value blob, function leaves become async trampolines produced by a fixed
factory with a runtime-assigned handle ID passed as a number, and the values
reach the module through V8's `import.meta` callback — no value is ever
printed into source text.

| Field       | Encoding                              | Notes                                                                                                                                                     |
| ----------- | ------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `specifier` | `String`                              | Import specifier.                                                                                                                                         |
| `kind`      | `u8`                                  | `0 = source`, `1 = host`.                                                                                                                                 |
| `source`    | `String`                              | kind 0 only. ESM source text, compiled fresh per isolate; transitive `import`s recurse back into the resolver.                                            |
| `exports`   | `List<(String name, HostModuleNode)>` | kind 1 only. Ordered top-level exports. Names must be valid JS identifiers (or `default`); the runtime re-validates before emitting them as export names. |

`HostModuleNode`:

| Tag Byte | Kind       | Tail                                 | Meaning                                                                                           |
| -------- | ---------- | ------------------------------------ | ------------------------------------------------------------------------------------------------- |
| `0x00`   | `function` | —                                    | Host function leaf; the runtime assigns handle IDs in tree-walk order over all declared bindings. |
| `0x01`   | `data`     | `ValueBlob value`                    | Constant materialised via the value codec (§4).                                                   |
| `0x02`   | `object`   | `List<(String key, HostModuleNode)>` | Nested plain object; may mix functions and data. Keys are plain property keys.                    |

Handle IDs never cross the wire: the runtime derives them from the declared
shape (depth-first over each binding, bindings in wire order) and resolves
them back to `(specifier, path)` before a `BridgeCall` frame is written.

`ImportRebind` (`PrefixRun` only):

The declared module shapes are frozen at declaration and stored with the
prefix; a `PrefixRun` sends only the **locations** of host function leaves
whose TS handler was replaced for this run. The runtime validates each
location against the declared shape and fails the run with
`ERR_UNDECLARED_BINDING` for anything that is not a declared host-module
function leaf (undeclared specifier or path, data leaf, source module) —
the same enforcement point that guards undeclared globals.

| Field       | Encoding | Notes                                                      |
| ----------- | -------- | ---------------------------------------------------------- |
| `specifier` | `String` | Declared host-module specifier.                            |
| `path`      | `String` | Dot-joined function-leaf path (e.g. `someObj.someMethod`). |

### 5.3 Prefix identifiers

`PrefixId`:

| Field | Encoding |
| ----- | -------- |
| `id`  | `String` |

### 5.4 Bridge payloads

`BridgeCallPayload`:

| Field        | Encoding           | Notes                                                                        |
| ------------ | ------------------ | ---------------------------------------------------------------------------- |
| `runId`      | `u32`              | The owning run — handlers are per run, and call ids alone cannot name it.    |
| `callId`     | `u32`              | Unique within one connection (the counter spans runs).                       |
| `targetKind` | `u8`               | `0 = global`, `1 = import`.                                                  |
| `specifier`  | `Optional<String>` | Import specifier when `targetKind = import`.                                 |
| `exportName` | `String`           | Global/stub name for globals; the dot-joined function-leaf path for imports. |
| `args`       | `ValueBlob`        | **One** blob holding the whole argument array (§4).                          |

Host-module function leaves dispatch through the reserved `__iso4_call`
bridge stub (installed by the runtime, never declared by the client) with
their handle ID as the first argument. The runtime resolves the ID against
its handle table before writing the frame: the payload carries
`targetKind = import` plus the resolved `specifier` and leaf path, with the
ID argument stripped — handle IDs never leave the runtime. A direct sandbox
call to `__iso4_call` with an invalid handle is refused without any I/O: the
call's promise rejects with a catchable host-bridge-style error and the
attempt is recorded as `blocked`.

`BridgeResponsePayload`:

| Field    | Encoding                    | Notes                                                                                                                                                                    |
| -------- | --------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `runId`  | `u32`                       | Echoed from the `BridgeCall` being answered — the session demux routes the response by it, statelessly. A response for a finished run fails the lookup and is discarded. |
| `callId` | `u32`                       | Must match the pending `BridgeCall`.                                                                                                                                     |
| `ok`     | `bool`                      | Whether the host handler succeeded.                                                                                                                                      |
| `value`  | `Optional<ValueBlob>`       | Present when `ok = true`; absent → `undefined`.                                                                                                                          |
| `error`  | `Optional<RunErrorPayload>` | Present when `ok = false`.                                                                                                                                               |

The `error` field is the bridge-error **subset** of `RunErrorPayload`:
`code` (always `ERR_HOST_BRIDGE`), `name`, `message`, the `stack` slot
(always absent — the host stack never crosses into the sandbox because it
can expose host file paths and infrastructure details), and `fields`. It
ends at `fields`: the trailing `reset` slot exists only on run-completion
errors (§5.6), never on bridge responses. `name`, `message`, and `fields` (all own-enumerable
properties of the thrown error beyond the reserved `name`/`message`/`stack`)
are carried — whatever a handler attaches to an error is the host's
responsibility, same as a returned value. Thrown primitives normalise to
`name = "Error"`, `message = String(value)`, no fields.

**Host handler errors are catchable.** Rust rejects the pending bridge Promise
with a real `Error` object rebuilt from the payload: built-in names like
`TypeError` use the matching intrinsic constructor (so `instanceof` works),
and every entry in `fields` is re-attached as a **direct own property** — the
caught error has the same shape the handler threw (`e.status`, `e.reason`, …).
Reserved keys (`name`/`message`/`stack`/`__proto__`) inside `fields` are
ignored so Error identity cannot be spoofed through the payload. Sandbox code
may catch it and continue running. Only if the rejection reaches the module's
top-level promise uncaught does the run fail with `ERR_HOST_BRIDGE`, with the
same fields preserved on the `RunErrorPayload`. Limit violations
(`ERR_BRIDGE_CALL_LIMIT_EXCEEDED`, `ERR_BRIDGE_PAYLOAD_TOO_LARGE`,
`ERR_FUNCTION_ARGUMENT_NOT_SUPPORTED`) remain fatal to the run even when
caught.

Within one run, several bridge calls can be in flight at once
(`Promise.all`): each `BridgeCall` returns a pending promise immediately, and
responses route back by `callId` in any order. Across runs, one connection can
carry several runs' bridge traffic simultaneously: the runtime's session demux
routes every run-tagged frame by its leading `runId`.

**`maxBridgeCallBytes` enforcement:** When non-zero, Rust checks the encoded
`BridgeCallPayload` byte length before writing it to the socket. If the payload
exceeds the limit the run terminates with `ERR_BRIDGE_PAYLOAD_TOO_LARGE` without
performing any I/O. That length is computed rather than materialized: every
field ahead of the args blob is fixed or already known, so the payload is
measured as header + blob and the two are written in sequence. The bytes on the
wire are exactly as laid out above; the args blob is never copied into a second
buffer, which would otherwise double the peak memory of every bridge call and
place that allocation before the check meant to bound it.

**BridgeResponse frame cap:** the sandbox cannot hold a response larger than
its own memory budget, so `memoryMb × 1 MiB` is the natural and only limit.
The session demux enforces it per run when routing the frame (the connection
read ceiling is the largest in-flight allowance); a frame over its run's cap
fails that run alone. When `memoryMb = 0` (unconstrained) the fallback is the
global 64 MiB `DEFAULT_MAX_FRAME_LENGTH`. There is no separate per-response
configuration field — to allow responses larger than 64 MiB, increase
`memoryMb`.

**`maxExportBytes` enforcement:** Rust copies the module namespace into a
plain object, serializes it once, and checks the resulting **blob** length
against `maxExportBytes`. If exceeded the run terminates with
`ERR_EXPORT_TOO_LARGE` before the `Result` frame is written. The limit is
measured on the bytes that actually cross the socket, so it costs nothing
extra; note a blob is roughly a third smaller than the codec it replaced at
dense payloads, so an existing `maxExportBytes` is now slightly more
permissive in terms of value count.

**`maxStdoutBytes` / `maxStderrBytes` enforcement:** Rust tracks running byte
totals in `LogBuffers`. Any console line whose addition would push the total
over the cap is silently dropped. The run itself continues normally.

**`maxBridgeCalls` enforcement:** When non-zero, Rust maintains a per-run
call counter shared across all bridge stubs. On each bridge call entry the
counter is incremented before any I/O. If the pre-increment value is already
at the limit, the run terminates with `ERR_BRIDGE_CALL_LIMIT_EXCEEDED` before
any frame is written to the socket. The runtime applies the default of `10`
when the caller leaves the field absent, so the limit is always active by
default; an explicit `0` disables it.

### 5.5 Streaming bodies

A `Request`/`Response` body that outgrows the host's probe (64 KiB) crosses
as a stream handle (§4.4.6) instead of inline bytes; small bodies keep the
buffered path unchanged. The bytes follow as frames on the run's connection,
interleaved with bridge traffic, under credit-based flow control. Streaming
applies to the per-run host → sandbox legs (call args, bridge responses);
data globals stay buffered because their values replay per instance. The
sandbox → host direction stays buffered in this version.

Every stream frame carries the **run id** alongside the stream id. Today one
run owns a connection at a time and the field is validated against the run in
flight; it exists so activating connection multiplexing later is a semantic
change, not a layout change.

**Flow control.** The runtime implicitly grants each stream an initial credit
window of 262144 bytes at hydration. The host may have at most that many
unconsumed bytes in flight; each chunk is at most 65536 bytes. As the sandbox
consumes bytes the runtime sends `StreamPull` frames replenishing exactly the
consumed count, so runtime-side buffering never exceeds the window (plus one
in-flight chunk of slack for benign races). A host exceeding the window is a
protocol fault that fails the run.

| Frame          | Direction | Payload                                                                  |
| -------------- | --------- | ------------------------------------------------------------------------ |
| `StreamChunk`  | TS → Rust | `u32 runId, u32 streamId, Bytes data` (≤ 65536 bytes)                    |
| `StreamEnd`    | TS → Rust | `u32 runId, u32 streamId, bool ok, Optional<String> error` (when not ok) |
| `StreamPull`   | Rust → TS | `u32 runId, u32 streamId, u32 credit`                                    |
| `StreamCancel` | Rust → TS | `u32 runId, u32 streamId, String reason`                                 |

Semantics: a clean `StreamEnd` resolves the sandbox's next read as EOF once
the buffer drains; a failed one rejects the pending read with a catchable
error carrying the message. `StreamCancel` (sandbox `reader.cancel()`, or the
run ending with the stream unread) tells the host to stop pumping and release
the source; chunks already in flight are dropped. Stream frames arriving for
a completed run are discarded. During a `waitUntil` grace phase (§5.8)
streams keep flowing like bridge traffic.

### 5.6 Diagnostic log payloads

`DiagnosticLogPayload` is for internal runtime diagnostics only. Sandbox
`console.*` output is captured by Rust and returned at the end of the run in
`RunSuccessPayload` or `RunFailurePayload`.

`DiagnosticLogPayload`:

| Field     | Encoding                                               |
| --------- | ------------------------------------------------------ |
| `level`   | `u8`: `0 = debug`, `1 = info`, `2 = warn`, `3 = error` |
| `message` | `String`                                               |

Sandbox `console.log`, `console.debug`, and `console.info` map to stdout.
`console.warn` and `console.error` map to stderr.

Prefix code shares that console. Output written while a warm instance
evaluates its prefix is carried on the completion of the call that
cold-started the instance (the run that paid for the warm-up) and cleared
afterwards, so later calls report only their own lines.

### 5.7 Completion payloads

`RunCompletionPayload`:

| Field     | Encoding                      | Notes                                                                                                                                                                                                                                                                |
| --------- | ----------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `runId`   | `u32`                         | Run being completed — the `runId` of the `Run`/`PrefixRun` this answers, echoed on every completion path. **Load-bearing, not advisory:** the host rejects a `Result` whose `runId` is not the one it sent, with `ERR_PROTOCOL_DESYNC`, and destroys the connection. |
| `ok`      | `bool`                        | Success or failure.                                                                                                                                                                                                                                                  |
| `success` | `Optional<RunSuccessPayload>` | Present when `ok = true`.                                                                                                                                                                                                                                            |
| `failure` | `Optional<RunFailurePayload>` | Present when `ok = false`.                                                                                                                                                                                                                                           |

`RunSuccessPayload`:

| Field            | Encoding                 | Notes                                                                                                                                                                                                                                       |
| ---------------- | ------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `exports`        | `ValueBlob`              | One blob holding a flat object: `default` plus named exports as direct properties. For a run that carried a `call` (§5.2) this is the called function's return value instead — the host knows which it asked for, so the slot needs no tag. |
| `skippedExports` | `List<String>`           | Export names absent from `exports` because their value cannot cross (a function, a Promise in any state — the export path never awaits — or a failed serialization). Skipping is never fatal. Always empty for a call run.                  |
| `stdout`         | `List<String>`           | Captured stdout log lines.                                                                                                                                                                                                                  |
| `stderr`         | `List<String>`           | Captured stderr log lines.                                                                                                                                                                                                                  |
| `durationMs`     | `f64`                    | Wall-clock runtime duration.                                                                                                                                                                                                                |
| `cpuTimeMs`      | `f64`                    | Active V8 execution time; bridge waits excluded.                                                                                                                                                                                            |
| `bridgeCalls`    | `List<BridgeCallRecord>` | One record per bridge call attempt, in attempt order.                                                                                                                                                                                       |
| `heapUsedBytes`  | `Optional<u64>`          | `used_heap_size` of the isolate that served the run, measured after it settled. Present for `PrefixRun` (warm instances — feeds eviction); absent for one-off `Run`, whose isolate is already gone.                                         |

`RunFailurePayload`:

| Field           | Encoding                 |
| --------------- | ------------------------ |
| `error`         | `RunErrorPayload`        |
| `stdout`        | `List<String>`           |
| `stderr`        | `List<String>`           |
| `durationMs`    | `f64`                    |
| `cpuTimeMs`     | `f64`                    |
| `bridgeCalls`   | `List<BridgeCallRecord>` |
| `heapUsedBytes` | `Optional<u64>`          |

`BridgeCallRecord` — per-call metadata (names, timing, sizes; never payloads):

| Field           | Encoding | Notes                                                                                                                                                       |
| --------------- | -------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `name`          | `String` | Public call name, resolved by the runtime: plain globals as-is (`fetch`), shims under their public name, host-module import leaves as `<specifier>.<path>`. |
| `startMs`       | `f64`    | Offset from run start (same clock as `durationMs`).                                                                                                         |
| `durationMs`    | `f64`    | Round-trip the sandbox waited; time-until-run-end for calls that never settled.                                                                             |
| `argBytes`      | `u32`    | Serialized call payload size in bytes (envelope + args blob) — what `maxBridgeCallBytes` is enforced against.                                               |
| `responseBytes` | `u32`    | Serialized response value size in bytes (the blob); `0` on handler error or unsettled.                                                                      |
| `ok`            | `bool`   | Handler resolved and the response reached the sandbox.                                                                                                      |
| `blocked`       | `bool`   | Blocked runtime-side (limit, oversized payload, function argument, invalid import handle); never sent.                                                      |

`RunErrorPayload`:

| Field     | Encoding              | Notes                                                                |
| --------- | --------------------- | -------------------------------------------------------------------- |
| `code`    | `String`              |                                                                      |
| `name`    | `String`              |                                                                      |
| `message` | `String`              |                                                                      |
| `stack`   | `Optional<String>`    | Always absent host → sandbox (BridgeResponse).                       |
| `fields`  | `Optional<ValueBlob>` | Own-enumerable props beyond `name`/`message`/`stack`, as one object. |
| `reset`   | `Optional<ResetInfo>` | Present only for `ERR_INSTANCE_RESET` (below).                       |

`ResetInfo` (the `ERR_INSTANCE_RESET` extras):

| Field          | Encoding | Notes                                                                                                                          |
| -------------- | -------- | ------------------------------------------------------------------------------------------------------------------------------ |
| `cause`        | `u8`     | `0 = cpu`, `1 = memory`, `2 = abort`, `3 = internal`, `4 = wall`.                                                              |
| `culpritRunId` | `u32`    | The wire run id of the run whose mid-execution interruption reset the shared instance; stable across the victims of one reset. |

`PrecompileResultPayload`:

| Field      | Encoding                    | Notes                      |
| ---------- | --------------------------- | -------------------------- |
| `ok`       | `bool`                      | Success or failure.        |
| `prefixId` | `Optional<PrefixId>`        | Present when `ok = true`.  |
| `error`    | `Optional<RunErrorPayload>` | Present when `ok = false`. |

### 5.8 RunComplete payloads

Sent exactly once, as the final frame of a run whose `Result` carried
`backgroundPending = true` — after the `waitUntil` grace phase ends. Grace
telemetry only: the run's own numbers already shipped on the `Result`.

`RunCompletePayload`:

| Field         | Encoding                                | Notes                                                                                                             |
| ------------- | --------------------------------------- | ----------------------------------------------------------------------------------------------------------------- |
| `runId`       | `u32`                                   | The run being completed; checked by the host like the Result's.                                                   |
| `status`      | `u8`                                    | `0 = settled` (all work finished), `1 = truncated` (budget or host abort), `2 = failed` (a registered rejection). |
| `durationMs`  | `f64`                                   | Grace wall time (after the Result shipped).                                                                       |
| `cpuTimeMs`   | `f64`                                   | Active V8 time during grace — metered separately from the run's.                                                  |
| `stdout`      | `List<String>`                          | Console lines written during grace.                                                                               |
| `stderr`      | `List<String>`                          |                                                                                                                   |
| `bridgeCalls` | `List<BridgeCallRecord>`                | Bridge calls **attempted** during grace (a call attempted pre-settle ships on the Result's records).              |
| `error`       | `Optional<String name, String message>` | The first rejection (`status = 2`), or the fatal error ending a truncated phase, when one exists.                 |

Between the `Result` and the `RunComplete` the connection still belongs to
the run: grace-time `BridgeCall`/`BridgeResponse` traffic flows exactly as
during the run, and the host must not start a new run on the slot until the
`RunComplete` arrives. This post-Result phase is the designed home for
future in-run streaming frames.

A `RunComplete` too large to frame is substituted by a minimal one — same
`runId` and `status`, empty telemetry, and an error naming the substitution —
mirroring the oversize recovery `Result` frames have, so an oversized grace
report costs its telemetry, never the connection. The carried error strings
are additionally capped at the runtime (256-byte name, 2048-byte message).

### 5.9 Stats payloads

A `Stats` request has an **empty payload** and is answered with exactly one
`StatsResult` frame — a point-in-time snapshot of the warm registry.
Counts are mutually consistent (taken under one registry lock) but stale the
moment they are sent: diagnostics, not synchronization. The host sends
`Stats` on a dedicated control connection outside its run pool, so a
snapshot answers even while every run slot is busy.

`StatsPayload`:

| Field             | Encoding            | Notes                                                                                                  |
| ----------------- | ------------------- | ------------------------------------------------------------------------------------------------------ |
| `oneoffRunning`   | `u32`               | Running one-off isolates (`Run` frames in flight).                                                     |
| `warmBusy`        | `u32`               | Warm instances currently serving a call.                                                               |
| `warmIdle`        | `u32`               | Idle warm instances ready for reuse.                                                                   |
| `idleHeapBytes`   | `u64`               | Summed `used_heap_size` of the idle instances, each measured after its last call.                      |
| `warmBudgetBytes` | `u64`               | The RSS mark the registry sheds against (`--warm-budget-bytes`). 0 = watermarks disabled.              |
| `rssBytes`        | `u64`               | The runtime process's resident set size at snapshot time; the signal the mark acts on. 0 = unreadable. |
| `underPressure`   | `u8`                | 1 while the shedding latch is held: RSS reached the budget, not yet back at 4/5 of it.                 |
| `prefixes`        | `List<PrefixStats>` | Per-prefix instance counts, sorted by prefix id.                                                       |

`PrefixStats`:

| Field      | Encoding   | Notes                                    |
| ---------- | ---------- | ---------------------------------------- |
| `prefixId` | `PrefixId` |                                          |
| `idle`     | `u32`      | Idle instances of this prefix.           |
| `busy`     | `u32`      | Instances of this prefix serving a call. |

---

## 6. Session lifecycle

Each connection handles one active run at a time. The TypeScript `Runtime`
maintains a pool of connections, one per `maxIsolates` slot, plus one
dedicated control connection used only for `Stats` — kept outside the
pool so a snapshot answers even while every run slot is busy. `Stats` is
legal on any authenticated connection (the runtime serves it in the
top-level message loop), but the host never sends it on a pooled connection.
Concurrency comes from multiple connections, not message-level
multiplexing.

```txt
TS (one connection slot)              Rust (one isolate thread)
│                                       │
│──── Authenticate ────────────────────▶│  version + V8 format check
│◀─── Hello ────────────────────────────│  handshake accepted (or refused)
│                                       │
│──── Run / PrefixRun ─────────────────▶│  create or restore isolate
│                                       │
│◀─── BridgeCall ───────────────────────│  sandbox called configured host fn
│──── BridgeResponse ──────────────────▶│  TS handler result/error
│                                       │
│◀─── Result ───────────────────────────│  final success/failure
│                                       │
│  next Run/PrefixRun may now begin     │
```

A run whose `Result` reports `backgroundPending = true` is not over:
the connection stays bound to it while `waitUntil` background work runs —
grace-time `BridgeCall`/`BridgeResponse` traffic included — until exactly one
`RunComplete` frame ends the run and frees the slot. The host resolves the
caller at the `Result` (that is the point of the feature) and keeps draining
the connection in the background.

`Precompile` uses the same authenticated connection but is not a run. It
validates the prefix (compile + instantiate + evaluate in a throwaway
isolate, under the fixed warm-up budget) and stores the **source and declared
shape** in the Rust process under a `PrefixId`. There is no runtime snapshot
(V8 14.x cannot create startup snapshots safely in a live multi-isolate
process; see DESIGN.md on the removal).

`PrefixRun` is served by **warm instances**: the runtime keeps a
registry of resident isolates per prefix, each owned by a dedicated runtime
thread. The first run cold-starts an instance (prefix evaluated under the
warm-up budget, never billed to the triggering run); later runs reuse it and
skip isolate boot and prefix evaluation entirely. This is invisible on the
wire — the frames are identical warm or cold; only `heapUsedBytes` on the
Result reports it. Instances are discarded on taint (any fired guard, abort
mid-call, fatal bridge error), on `DisposePrefix`, and by scored eviction
under memory pressure: there is no instance-count cap, only the RSS mark
(`--warm-budget-bytes`). At/above the mark the runtime evicts idle
instances by `heapUsed × idleTime` and stops admitting new warmth until RSS
falls back to 4/5 of the mark, so a `PrefixRun` with no idle instance
available runs on a fresh one-off isolate instead of pooling another. Idle
instances can therefore outnumber concurrent runs, and their number follows
memory rather than the pool size. State inside one instance survives between
its calls as a cache, never a guarantee; instances of one prefix share no
state with each other. One-off `Run` frames always get a fresh isolate.

---

## 7. Error codes

| Code                                  | Cause                                                                                                                                                                                                                                                                                                                                                                                                                                                                       |
| ------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `ERR_USER_CODE`                       | Uncaught exception or rejected top-level await in sandbox JS.                                                                                                                                                                                                                                                                                                                                                                                                               |
| `ERR_MEMORY_LIMIT`                    | V8 heap + ArrayBuffer exceeded the isolate's heap cap (`memoryMb` — a Runtime-level setting, carried in the limits slot of every frame by the host).                                                                                                                                                                                                                                                                                                                        |
| `ERR_CPU_TIMEOUT`                     | Active JS execution exceeded `limits.cpuTimeMs`.                                                                                                                                                                                                                                                                                                                                                                                                                            |
| `ERR_WALL_TIMEOUT`                    | Total runtime exceeded `limits.wallTimeMs`.                                                                                                                                                                                                                                                                                                                                                                                                                                 |
| `ERR_ABORTED`                         | Host aborted the run (sent `Terminate` after its `AbortSignal` fired).                                                                                                                                                                                                                                                                                                                                                                                                      |
| `ERR_MODULE_NOT_FOUND`                | Import specifier not in the resolved import set.                                                                                                                                                                                                                                                                                                                                                                                                                            |
| `ERR_COMPILE`                         | Syntax/module compile error.                                                                                                                                                                                                                                                                                                                                                                                                                                                |
| `ERR_FUNCTION_ARGUMENT_NOT_SUPPORTED` | Function argument attempted to cross the host bridge.                                                                                                                                                                                                                                                                                                                                                                                                                       |
| `ERR_EXPORT_NOT_SERIALIZABLE`         | A call's return value (or bridge value) holds something V8 cannot clone — see §4.2. Non-serializable _exports_ are no longer fatal: they are skipped and reported in `skippedExports` (§5.7).                                                                                                                                                                                                                                                                               |
| `ERR_CALL_TARGET_NOT_FOUND`           | A `call.exportPath` (§5.2) does not resolve against the module's exports, or resolves to a value that is not callable. The message says which and names the path.                                                                                                                                                                                                                                                                                                           |
| `ERR_EXPORT_TOO_LARGE`                | Encoded exports exceed `limits.maxExportBytes`.                                                                                                                                                                                                                                                                                                                                                                                                                             |
| `ERR_HOST_BRIDGE`                     | Host global/import handler threw or rejected, uncaught by sandbox code.                                                                                                                                                                                                                                                                                                                                                                                                     |
| `ERR_BRIDGE_PAYLOAD_TOO_LARGE`        | Bridge call payload exceeded `limits.maxBridgeCallBytes`.                                                                                                                                                                                                                                                                                                                                                                                                                   |
| `ERR_BRIDGE_CALL_LIMIT_EXCEEDED`      | Total bridge calls in this run exceeded `limits.maxBridgeCalls`.                                                                                                                                                                                                                                                                                                                                                                                                            |
| `ERR_TYPE_NOT_SERIALIZABLE`           | A registered host type cannot cross — an unimplemented tag, or contents that are not self-contained (a body that is not `null`/string/`Uint8Array`, `WebSocket`, `AbortSignal`). See §4.4.5.                                                                                                                                                                                                                                                                                |
| `ERR_UNDECLARED_BINDING`              | `PrefixRun` attempted to bind a global/import not declared by `Precompile`.                                                                                                                                                                                                                                                                                                                                                                                                 |
| `ERR_PREFIX_DID_NOT_SETTLE`           | Prefix top-level evaluation stayed pending after the microtask queue drained — nothing in the isolate can resolve the awaited promise at `Precompile` time.                                                                                                                                                                                                                                                                                                                 |
| `ERR_PREFIX_BRIDGE_CALL`              | Prefix code called a bridge callable (bridge global, shim global, or host-import function). The bridge does not exist while a prefix evaluates — at `Precompile` validation or at a run's prefix stage.                                                                                                                                                                                                                                                                     |
| `ERR_WARMUP_LIMIT`                    | Prefix evaluation (plus the per-instance runtime installs) exceeded its fixed warm-up budget (1 s wall / 1 s CPU, not configurable — Cloudflare's script-startup model). Isolate boot itself precedes the budget. Enforced at `Precompile` and at instance cold-start. Move expensive setup into the handler (lazy init on first call).                                                                                                                                     |
| `ERR_PREFIX_DISPOSED`                 | Prefix was disposed or evicted.                                                                                                                                                                                                                                                                                                                                                                                                                                             |
| `ERR_INSTANCE_RESET`                  | The run was an innocent victim: a co-resident run on the same shared instance was interrupted mid-execution (a CPU/memory/wall guard fired, or a forced abort landed on running code), so the instance could no longer be trusted and every run in flight on it failed. Carries `ResetInfo` (§5.6): the cause class and the culprit's wire run id. Telemetry fields are this run's real partial values. Never retried automatically — the victim may have had side effects. |
| `ERR_PROTOCOL_DESYNC`                 | **Host-detected, never sent by the runtime.** The host read a `Result` whose `runId` is not the one it sent, or a frame with no place in the run protocol, so the two sides lost frame alignment. The displaced run never reached an isolate: telemetry is zero rather than partial, and the connection is destroyed rather than reused. See §5.7.                                                                                                                          |
| `ERR_INTERNAL`                        | Runtime bug or unexpected host/runtime failure.                                                                                                                                                                                                                                                                                                                                                                                                                             |

---

## 8. Versioning

Two independent versions are checked at handshake time (§5.1), both fatal:

1. **The iso4 protocol version** — the `u16` in the `Authenticate` frame.
   Bump it whenever the frame envelope, message tables, or payload layouts
   change incompatibly.
2. **The V8 serialization format version** — carried in each side's probe.
   Not ours to bump: V8 changes it, and neither Node nor rusty_v8 lets an
   embedder pin what it writes. The runtime accepts a host that writes a
   format it can read (`hostVersion <= runtimeWriteVersion`), and the host
   proves the reverse direction by deserializing the runtime's probe.

`@iso4/sandbox` and every `@iso4/v8-*` package must be released together on
an incompatible protocol change — and, because the V8 format version rides
along with whichever V8 each side embeds, on any V8 bump too. That lockstep
release policy is what makes the hard-fail handshake acceptable: a mismatch
means the two halves were installed out of sync, which the error message says
outright.
