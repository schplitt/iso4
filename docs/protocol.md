# iso4 Wire Protocol

Communication between the TypeScript host (`@iso4/sandbox`) and the Rust V8
binary (`iso4-v8`) happens over a Unix domain socket using length-prefixed
binary frames. The frame envelope is small and stable; structured message
payloads use the iso4 binary `WireValue` codec defined below.

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

Current protocol version: **`1`**.

---

## 2. Message tables

### 2.1 TS → Rust

|   Byte | Name             | Payload                 | Response                                             |
| -----: | ---------------- | ----------------------- | ---------------------------------------------------- |
| `0x01` | `Authenticate`   | `AuthenticatePayload`   | no frame; Rust closes the socket on mismatch         |
| `0x02` | `Run`            | `RunPayload`            | zero or more `BridgeCall`, then exactly one `Result` |
| `0x03` | `Precompile`     | `PrecompilePayload`     | exactly one `PrecompileResult`                       |
| `0x04` | `PrefixRun`      | `PrefixRunPayload`      | zero or more `BridgeCall`, then exactly one `Result` |
| `0x05` | `DisposePrefix`  | `PrefixId`              | no frame; idempotent                                 |
| `0x06` | `BridgeResponse` | `BridgeResponsePayload` | resumes the waiting sandbox bridge call              |
| `0x07` | `Terminate`      | `RunId`                 | Rust sends one `Result` with `ERR_TERMINATED`        |

### 2.2 Rust → TS

|   Byte | Name               | Payload                   | Notes                                                                                                 |
| -----: | ------------------ | ------------------------- | ----------------------------------------------------------------------------------------------------- |
| `0x01` | `BridgeCall`       | `BridgeCallPayload`       | Sandbox called a configured host global/function or host import.                                      |
| `0x02` | `Result`           | `RunCompletionPayload`    | Final completion for `Run` or `PrefixRun`; sent exactly once. Includes captured stdout/stderr arrays. |
| `0x03` | `PrecompileResult` | `PrecompileResultPayload` | Result of `Precompile`.                                                                               |
| `0x04` | `Log`              | `DiagnosticLogPayload`    | Internal runtime diagnostic; not sandbox stdout/stderr.                                               |

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

Strings MUST be valid UTF-8. Decoders MUST reject invalid booleans and invalid
optional presence bytes.

---

## 4. WireValue codec

`WireValue` is the data-only value format used for exports, bridge arguments,
and bridge return values. It is intentionally independent of V8’s internal
serializer so the TypeScript host can decode it without native APIs.

### 4.1 Value tags

|    Tag | Name        | Payload             | Decodes to Ts |
| -----: | ----------- | ------------------- | ------------- |
| `0x00` | `Undefined` | none                | `undefined`   |
| `0x01` | `Null`      | none                | `null`        |
| `0x02` | `False`     | none                | `false`       |
| `0x03` | `True`      | none                | `true`        |
| `0x04` | `Number`    | `f64`               | `number`      |
| `0x05` | `String`    | `String`            | `string`      |
| `0x06` | `BigInt`    | see below           | `bigint`      |
| `0x07` | `Bytes`     | `Bytes`             | `Uint8Array`  |
| `0x08` | `Array`     | `List<WireValue>`   | `unknown[]`   |
| `0x09` | `Object`    | `List<ObjectField>` | plain object  |

`BigInt` payload:

| Field        | Encoding                                  | Notes                                                                         |
| ------------ | ----------------------------------------- | ----------------------------------------------------------------------------- |
| `sign_bit`   | `u8` (`0` = non-negative, `1` = negative) | Always `0` for zero.                                                          |
| `word_count` | `u32`                                     | Number of 64-bit words that follow. `0` for zero.                             |
| `words`      | `word_count × u64` (big-endian each)      | Least-significant word first (index 0 = bits 0–63, index 1 = bits 64–127, …). |

This encoding maps directly to V8's `BigInt::new_from_words` / `to_words_array` API — no base conversion is needed on either side of the bridge. The TypeScript side uses native `bigint` bit-shift arithmetic to pack/unpack words.

`ObjectField`:

| Field   | Encoding    |
| ------- | ----------- |
| `key`   | `String`    |
| `value` | `WireValue` |

### 4.2 Value extraction rules

Rust MUST reject the following when extracting sandbox values:

| Js Value                  | Error                                                                                        |
| ------------------------- | -------------------------------------------------------------------------------------------- |
| function                  | `ERR_EXPORT_NOT_SERIALIZABLE` or `ERR_FUNCTION_ARGUMENT_NOT_SUPPORTED` depending on boundary |
| unresolved Promise        | `ERR_EXPORT_UNRESOLVED_PROMISE` for exports                                                  |
| Symbol                    | `ERR_EXPORT_NOT_SERIALIZABLE`                                                                |
| cyclic object/array graph | `ERR_EXPORT_NOT_SERIALIZABLE`                                                                |

Objects are serialized as own enumerable string-keyed properties only. Prototype
methods and non-enumerable properties are not serialized.

The key `"__proto__"` is **silently elided in both directions**:

- **Sandbox → host** (`serialize_object_fields` in `v8.rs`): dropped before the
  `BridgeCall` or export payload is encoded.
- **Host → sandbox** (`wire_to_v8_value` in `v8.rs`): dropped before the value
  is injected into the V8 object.

The TS WireValue encoder (`encodeWireValue`) and decoder (`decodeWireValue`)
apply the same guard for defence-in-depth.

A host returning `{ "__proto__": { polluted: true }, x: 1 }` delivers only
`{ x: 1 }` to the sandbox. A sandbox exporting
`Object.defineProperty({}, "__proto__", { value: 1, enumerable: true })`
delivers only `{}` to the host. The key is not re-encoded under a mangled
safe name — it is simply dropped.

### 4.3 Encoding examples

#### Example: nested object and array export

Sandbox code:

```js
export const someExport = { hello: ['some', 123] }
```

Public TypeScript result shape:

```ts
const result = {
  ok: true,
  exports: {
    someExport: {
      hello: ['some', 123],
    },
  },
  stdout: [],
  stderr: [],
  durationMs: 1,
}
```

Wire representation of only the `exports` value:

```txt
WireValue::Object
└─ field count: 1
   └─ key: "someExport"
      value: WireValue::Object
      └─ field count: 1
         └─ key: "hello"
            value: WireValue::Array
            └─ item count: 2
               ├─ WireValue::String "some"
               └─ WireValue::Number 123.0
```

Byte-level layout of that `exports` value:

```txt
09                                  # Object
00 00 00 01                         # 1 field
00 00 00 0a 73 6f 6d 65 45 78 70 6f 72 74
                                    # key "someExport"
09                                  # Object
00 00 00 01                         # 1 field
00 00 00 05 68 65 6c 6c 6f          # key "hello"
08                                  # Array
00 00 00 02                         # 2 items
05                                  # String
00 00 00 04 73 6f 6d 65             # "some"
04                                  # Number
40 5e c0 00 00 00 00 00             # f64 123.0
```

#### Example: default plus named exports

Sandbox code:

```js
export default { ok: true }
export const count = 2
```

The `exports` payload is a single flat object. `default` is not a separate
field in the run result; it is just the property named `"default"`:

```txt
WireValue::Object
└─ field count: 2
   ├─ key: "default"
   │  value: WireValue::Object
   │  └─ key: "ok"
   │     value: WireValue::True
   └─ key: "count"
      value: WireValue::Number 2.0
```

This decodes to:

```ts
const result = {
  default: { ok: true },
  count: 2,
}
```

---

## 5. Message payload schemas

### 5.1 Authentication

`AuthenticatePayload`:

| Field             | Encoding                                     |
| ----------------- | -------------------------------------------- |
| `protocolVersion` | `u16`                                        |
| `token`           | UTF-8 bytes for the remainder of the payload |

Authentication MUST be the first frame on every connection. Rust closes the
socket immediately on version or token mismatch.

### 5.2 Run payloads

`RunPayload`:

| Field      | Encoding                  | Notes                                  |
| ---------- | ------------------------- | -------------------------------------- |
| `runId`    | `u32`                     | Unique on this connection.             |
| `code`     | `String`                  | ESM source.                            |
| `filename` | `Optional<String>`        | Used in stack traces.                  |
| `limits`   | `ResourceLimits`          | Fully normalized by TS before sending. |
| `globals`  | `List<HostGlobalBinding>` | Names configured for this run.         |
| `imports`  | `List<ImportBinding>`     | Source or host import declarations.    |

`PrefixRunPayload`:

| Field      | Encoding                  | Notes                                         |
| ---------- | ------------------------- | --------------------------------------------- |
| `runId`    | `u32`                     | Unique on this connection.                    |
| `prefixId` | `PrefixId`                | Snapshot handle returned by `Precompile`.     |
| `code`     | `String`                  | ESM postfix source.                           |
| `filename` | `Optional<String>`        | Used in stack traces.                         |
| `limits`   | `ResourceLimits`          | Fully normalized by TS before sending.        |
| `globals`  | `List<HostGlobalBinding>` | Must be subset of predeclared globals.        |
| `imports`  | `List<ImportBinding>`     | Rebindings for predeclared host imports only. |

`PrecompilePayload`:

| Field      | Encoding                  | Notes                                                              |
| ---------- | ------------------------- | ------------------------------------------------------------------ |
| `code`     | `String`                  | ESM prefix source.                                                 |
| `filename` | `Optional<String>`        | Used in stack traces.                                              |
| `limits`   | `ResourceLimits`          | Limits used during precompile.                                     |
| `globals`  | `List<HostGlobalBinding>` | Declares permitted global shape.                                   |
| `imports`  | `List<ImportBinding>`     | Source imports are snapshotted; host imports declare bridge shape. |

`ResourceLimits`:

| Field                   | Encoding | Notes                                                                                                                                                                                                             |
| ----------------------- | -------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `memoryMb`              | `u32`    | Zero = no limit.                                                                                                                                                                                                  |
| `cpuTimeMs`             | `u32`    | Zero = no limit.                                                                                                                                                                                                  |
| `wallTimeMs`            | `u32`    | Zero = no limit.                                                                                                                                                                                                  |
| `maxExportBytes`        | `u32`    | Zero = no limit.                                                                                                                                                                                                  |
| `maxStdoutBytes`        | `u32`    | Zero = no limit.                                                                                                                                                                                                  |
| `maxStderrBytes`        | `u32`    | Zero = no limit.                                                                                                                                                                                                  |
| `maxBridgePayloadBytes` | `u32`    | Max byte length of a single `BridgeCallPayload` or `BridgeResponsePayload`. Zero = no limit (framing cap of 64 MiB applies). Violation → `ERR_BRIDGE_PAYLOAD_TOO_LARGE`.                                          |
| `maxBridgeCalls`        | `u32`    | Maximum total bridge calls (globals + host imports combined) a single run may make. Zero = no limit. TS default: `10` when the host does not set an explicit value. Violation → `ERR_BRIDGE_CALL_LIMIT_EXCEEDED`. |

`HostGlobalBinding`:

| Field  | Encoding | Notes                                                  |
| ------ | -------- | ------------------------------------------------------ |
| `name` | `String` | Example: `fetch`; only allowlisted names are accepted. |

`ImportBinding`:

| Field         | Encoding           | Notes                                                    |
| ------------- | ------------------ | -------------------------------------------------------- |
| `specifier`   | `String`           | Import specifier.                                        |
| `kind`        | `u8`               | `0 = source`, `1 = host`.                                |
| `source`      | `Optional<String>` | Present only when `kind = source`.                       |
| `hostExports` | `List<String>`     | Export names for host import modules when `kind = host`. |

### 5.3 Prefix identifiers

`PrefixId`:

| Field | Encoding |
| ----- | -------- |
| `id`  | `String` |

### 5.4 Bridge payloads

`BridgeCallPayload`:

| Field        | Encoding           | Notes                                        |
| ------------ | ------------------ | -------------------------------------------- |
| `callId`     | `u32`              | Unique within one run.                       |
| `targetKind` | `u8`               | `0 = global`, `1 = import`.                  |
| `specifier`  | `Optional<String>` | Import specifier when `targetKind = import`. |
| `exportName` | `String`           | Function/global name.                        |
| `args`       | `List<WireValue>`  | Function arguments.                          |

`BridgeResponsePayload`:

| Field    | Encoding                    | Notes                                |
| -------- | --------------------------- | ------------------------------------ |
| `callId` | `u32`                       | Must match the pending `BridgeCall`. |
| `ok`     | `bool`                      | Whether the host handler succeeded.  |
| `value`  | `Optional<WireValue>`       | Present when `ok = true`.            |
| `error`  | `Optional<RunErrorPayload>` | Present when `ok = false`.           |

Bridge calls are sequential within a single run in v1: Rust sends one
`BridgeCall` and waits for the matching `BridgeResponse` before continuing JS
execution.

**`maxBridgePayloadBytes` enforcement:** When non-zero, Rust checks the encoded
`BridgeCallPayload` byte length before writing it to the socket. If the payload
exceeds the limit the run terminates with `ERR_BRIDGE_PAYLOAD_TOO_LARGE` without
performing any I/O. Rust also checks the `BridgeResponsePayload` byte length
immediately after reading the frame; if it exceeds the limit the run terminates
with `ERR_BRIDGE_PAYLOAD_TOO_LARGE` before decoding the payload. The fallback
cap is the framing layer's 64 MiB `DEFAULT_MAX_FRAME_LENGTH`.

**`maxBridgeCalls` enforcement:** When non-zero, Rust maintains a per-run
call counter shared across all bridge stubs. On each bridge call entry the
counter is incremented before any I/O. If the pre-increment value is already
at the limit, the run terminates with `ERR_BRIDGE_CALL_LIMIT_EXCEEDED` before
any frame is written to the socket. The TS encoder sends `10` when the host
does not set an explicit value, so the limit is always active by default.

### 5.5 Diagnostic log payloads

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

### 5.6 Completion payloads

`RunCompletionPayload`:

| Field     | Encoding                      | Notes                      |
| --------- | ----------------------------- | -------------------------- |
| `runId`   | `u32`                         | Run being completed.       |
| `ok`      | `bool`                        | Success or failure.        |
| `success` | `Optional<RunSuccessPayload>` | Present when `ok = true`.  |
| `failure` | `Optional<RunFailurePayload>` | Present when `ok = false`. |

`RunSuccessPayload`:

| Field        | Encoding            | Notes                                                       |
| ------------ | ------------------- | ----------------------------------------------------------- |
| `exports`    | `WireValue::Object` | Contains `default` plus named exports as direct properties. |
| `stdout`     | `List<String>`      | Captured stdout log lines.                                  |
| `stderr`     | `List<String>`      | Captured stderr log lines.                                  |
| `durationMs` | `f64`               | Total runtime duration.                                     |

`RunFailurePayload`:

| Field        | Encoding          |
| ------------ | ----------------- |
| `error`      | `RunErrorPayload` |
| `stdout`     | `List<String>`    |
| `stderr`     | `List<String>`    |
| `durationMs` | `f64`             |

`RunErrorPayload`:

| Field     | Encoding           |
| --------- | ------------------ |
| `code`    | `String`           |
| `name`    | `String`           |
| `message` | `String`           |
| `stack`   | `Optional<String>` |

`PrecompileResultPayload`:

| Field      | Encoding                    | Notes                      |
| ---------- | --------------------------- | -------------------------- |
| `ok`       | `bool`                      | Success or failure.        |
| `prefixId` | `Optional<PrefixId>`        | Present when `ok = true`.  |
| `error`    | `Optional<RunErrorPayload>` | Present when `ok = false`. |

---

## 6. Session lifecycle

Each connection handles one active run at a time. The TypeScript `Runtime`
maintains a pool of connections, one per `maxIsolates` slot. Concurrency comes
from multiple connections, not message-level multiplexing.

```txt
TS (one connection slot)              Rust (one isolate thread)
│                                       │
│──── Authenticate ────────────────────▶│  version + token check
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

`Precompile` uses the same authenticated connection but is not a run. It
returns `PrecompileResult` and stores the snapshot in the Rust process under a
`PrefixId`.

---

## 7. Error codes

| Code                                  | Cause                                                                       |
| ------------------------------------- | --------------------------------------------------------------------------- |
| `ERR_USER_CODE`                       | Uncaught exception or rejected top-level await in sandbox JS.               |
| `ERR_MEMORY_LIMIT`                    | V8 heap + ArrayBuffer exceeded `limits.memoryMb`.                           |
| `ERR_CPU_TIMEOUT`                     | Active JS execution exceeded `limits.cpuTimeMs`.                            |
| `ERR_WALL_TIMEOUT`                    | Total runtime exceeded `limits.wallTimeMs`.                                 |
| `ERR_ABORTED`                         | Host aborted the run.                                                       |
| `ERR_TERMINATED`                      | Host sent `Terminate`.                                                      |
| `ERR_MODULE_NOT_FOUND`                | Import specifier not in the resolved import set.                            |
| `ERR_COMPILE`                         | Syntax/module compile error.                                                |
| `ERR_FUNCTION_ARGUMENT_NOT_SUPPORTED` | Function argument attempted to cross the host bridge.                       |
| `ERR_EXPORT_NOT_SERIALIZABLE`         | Export contains unsupported value or cycle.                                 |
| `ERR_EXPORT_TOO_LARGE`                | Encoded exports exceed `limits.maxExportBytes`.                             |
| `ERR_EXPORT_UNRESOLVED_PROMISE`       | Export value is a pending Promise.                                          |
| `ERR_HOST_BRIDGE`                     | Configured host global/import handler threw or rejected.                    |
| `ERR_BRIDGE_PAYLOAD_TOO_LARGE`        | Bridge call or response payload exceeded `limits.maxBridgePayloadBytes`.    |
| `ERR_BRIDGE_CALL_LIMIT_EXCEEDED`      | Total bridge calls in this run exceeded `limits.maxBridgeCalls`.            |
| `ERR_UNDECLARED_BINDING`              | `PrefixRun` attempted to bind a global/import not declared by `Precompile`. |
| `ERR_PREFIX_DISPOSED`                 | Prefix snapshot was disposed or evicted.                                    |
| `ERR_INTERNAL`                        | Runtime bug or unexpected host/runtime failure.                             |

---

## 8. Versioning

The `Authenticate` frame carries the `u16` protocol version. Rust closes the
socket immediately on mismatch because the connection is untrusted before auth
succeeds.

Bump the protocol version whenever the frame envelope, message tables, or
payload codecs change incompatibly. `@iso4/sandbox` and every `@iso4/v8-*`
package must be released together on an incompatible protocol change.
