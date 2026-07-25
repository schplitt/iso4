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

|   Byte | Name             | Payload                 | Response                                                                                                                         |
| -----: | ---------------- | ----------------------- | -------------------------------------------------------------------------------------------------------------------------------- |
| `0x01` | `Authenticate`   | `AuthenticatePayload`   | no frame; Rust closes the socket on mismatch                                                                                     |
| `0x02` | `Run`            | `RunPayload`            | zero or more `BridgeCall`, then exactly one `Result`                                                                             |
| `0x03` | `Precompile`     | `PrecompilePayload`     | exactly one `PrecompileResult`                                                                                                   |
| `0x04` | `PrefixRun`      | `PrefixRunPayload`      | zero or more `BridgeCall`, then exactly one `Result`                                                                             |
| `0x05` | `DisposePrefix`  | `PrefixId`              | no frame; idempotent                                                                                                             |
| `0x06` | `BridgeResponse` | `BridgeResponsePayload` | resumes the waiting sandbox bridge call                                                                                          |
| `0x07` | `Terminate`      | `RunId`                 | Rust sends one `Result` with `ERR_ABORTED` (graceful abort); a CPU-bound run not reading frames is instead reclaimed by teardown |

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

| Field      | Encoding              | Notes                                                |
| ---------- | --------------------- | ---------------------------------------------------- |
| `runId`    | `u32`                 | Unique on this connection.                           |
| `code`     | `String`              | ESM source.                                          |
| `filename` | `Optional<String>`    | Used in stack traces.                                |
| `limits`   | `ResourceLimits`      | Only caller-set fields sent; runtime fills defaults. |
| `globals`  | `List<GlobalDef>`     | Host globals + how the runtime installs each one.    |
| `imports`  | `List<ImportBinding>` | Source or host import declarations.                  |

`PrefixRunPayload`:

| Field      | Encoding             | Notes                                                                                             |
| ---------- | -------------------- | ------------------------------------------------------------------------------------------------- |
| `runId`    | `u32`                | Unique on this connection.                                                                        |
| `prefixId` | `PrefixId`           | Snapshot handle returned by `Precompile`.                                                         |
| `code`     | `String`             | ESM postfix source.                                                                               |
| `filename` | `Optional<String>`   | Used in stack traces.                                                                             |
| `limits`   | `ResourceLimits`     | Fully normalized by TS before sending.                                                            |
| `globals`  | `List<GlobalDef>`    | Bridge stubs to re-install; subset of predeclared. Always `bridge` kind (values are snapshotted). |
| `imports`  | `List<ImportRebind>` | Locations of host-import function leaves whose handler was replaced for this run.                 |

`PrecompilePayload`:

| Field      | Encoding              | Notes                                                               |
| ---------- | --------------------- | ------------------------------------------------------------------- |
| `code`     | `String`              | ESM prefix source.                                                  |
| `filename` | `Optional<String>`    | Used in stack traces.                                               |
| `limits`   | `ResourceLimits`      | Limits used during precompile.                                      |
| `globals`  | `List<GlobalDef>`     | Declares the global shape; value kinds are baked into the snapshot. |
| `imports`  | `List<ImportBinding>` | Source imports are snapshotted; host imports declare bridge shape.  |

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
| `maxExportBytes`     | `Optional<u32>` | `16 MiB` | Max serialised byte length of the export `WireValue`. Zero = no limit. Violation → `ERR_EXPORT_TOO_LARGE`.                                                       |
| `maxStdoutBytes`     | `Optional<u32>` | `1 MiB`  | Max bytes captured across all stdout lines. Zero = no limit. Lines that would exceed the cap are silently dropped.                                               |
| `maxStderrBytes`     | `Optional<u32>` | `1 MiB`  | Max bytes captured across all stderr lines. Zero = no limit. Lines that would exceed the cap are silently dropped.                                               |
| `maxBridgeCallBytes` | `Optional<u32>` | `16 MiB` | Max byte length of a single `BridgeCallPayload` (sandbox → host args). Zero = no limit (64 MiB framing cap applies). Violation → `ERR_BRIDGE_PAYLOAD_TOO_LARGE`. |
| `maxBridgeCalls`     | `Optional<u32>` | `10`     | Maximum total bridge calls (globals + host imports combined) a single run may make. Zero = no limit. Violation → `ERR_BRIDGE_CALL_LIMIT_EXCEEDED`.               |

`GlobalDef`:

Each host global is installed **natively** by the runtime — the client never
prepends generated source to user code, so user code always starts at line 1,
and a global's name reaches the sandbox global object through the V8 API
(`object.set`), never interpolated into an identifier position. Every entry is a
`u8` kind tag, then a `String` name, then a kind-specific tail:

| Kind Byte | Kind     | Tail                              | Install                                                                                                                            |
| --------- | -------- | --------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------- |
| `0x00`    | `bridge` | —                                 | Bridge stub under `name` (issues `BridgeCall` frames).                                                                             |
| `0x01`    | `string` | `String expr`                     | Runtime evaluates `(expr)` as its own script; sets `globalThis[name]`.                                                             |
| `0x02`    | `data`   | `WireValue value`                 | Materialised via the value codec; sets `globalThis[name]`.                                                                         |
| `0x03`    | `shim`   | `String shim, String handlerName` | Installs a bridge stub under `handlerName` and a wrapper `async (...a) => shim(await globalThis[handlerName](...a))` under `name`. |

Only `bridge` and `shim` install a bridge stub (and so require the session
socket); `string`/`data` are pure in-isolate installs. On `PrefixRun` every
entry is `bridge` kind — string/data globals and shim wrappers are baked into
the snapshot at `Precompile` time, so only their bridge stubs are re-installed
per run.

`ImportBinding` (`Run` / `Precompile`):

Both public import flavors cross the wire structurally — the client never
generates sandbox source. A source module carries ESM text verbatim; a host
module carries its **shape** as a tree of data. The runtime builds host
modules natively (see DESIGN.md §4.3): data leaves are materialised with the
value codec, function leaves become async trampolines produced by a fixed
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

| Tag Byte | Kind       | Tail                                 | Meaning                                                                                                                    |
| -------- | ---------- | ------------------------------------ | -------------------------------------------------------------------------------------------------------------------------- |
| `0x00`   | `function` | —                                    | Host function leaf; the runtime assigns handle IDs in tree-walk order over all declared bindings.                          |
| `0x01`   | `data`     | `WireValue value`                    | Constant materialised via the value codec.                                                                                 |
| `0x02`   | `object`   | `List<(String key, HostModuleNode)>` | Nested plain object; may mix functions and data. Keys are property keys (any string except `__proto__`, which is dropped). |

Handle IDs never cross the wire: the runtime derives them from the declared
shape (depth-first over each binding, bindings in wire order) and resolves
them back to `(specifier, path)` before a `BridgeCall` frame is written.

`ImportRebind` (`PrefixRun` only):

The declared module shapes are frozen with the snapshot and stored with the
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
| `callId`     | `u32`              | Unique within one run.                                                       |
| `targetKind` | `u8`               | `0 = global`, `1 = import`.                                                  |
| `specifier`  | `Optional<String>` | Import specifier when `targetKind = import`.                                 |
| `exportName` | `String`           | Global/stub name for globals; the dot-joined function-leaf path for imports. |
| `args`       | `List<WireValue>`  | Function arguments.                                                          |

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

| Field    | Encoding                    | Notes                                |
| -------- | --------------------------- | ------------------------------------ |
| `callId` | `u32`                       | Must match the pending `BridgeCall`. |
| `ok`     | `bool`                      | Whether the host handler succeeded.  |
| `value`  | `Optional<WireValue>`       | Present when `ok = true`.            |
| `error`  | `Optional<RunErrorPayload>` | Present when `ok = false`.           |

The `error` field uses the `RunErrorPayload` layout with `code` always
`ERR_HOST_BRIDGE` and the `stack` slot always absent: the host stack never
crosses into the sandbox because it can expose host file paths and
infrastructure details. `name`, `message`, and `fields` (all own-enumerable
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

Bridge calls are sequential within a single run in v1: Rust sends one
`BridgeCall` and waits for the matching `BridgeResponse` before continuing JS
execution.

**`maxBridgeCallBytes` enforcement:** When non-zero, Rust checks the encoded
`BridgeCallPayload` byte length before writing it to the socket. If the payload
exceeds the limit the run terminates with `ERR_BRIDGE_PAYLOAD_TOO_LARGE` without
performing any I/O.

**BridgeResponse frame cap:** `BridgeResponse` frames are read with
`read_frame_with_limit(memoryMb × 1 MiB)`. The sandbox cannot hold a response
larger than its own memory budget, so `memoryMb` is the natural and only limit.
When `memoryMb = 0` (unconstrained) the fallback is the global 64 MiB
`DEFAULT_MAX_FRAME_LENGTH`. There is no separate per-response configuration
field — to allow responses larger than 64 MiB, increase `memoryMb`.

**`maxExportBytes` enforcement:** After all exports are serialised to a `WireValue`,
Rust encodes the value to bytes and checks the length against `maxExportBytes`.
If exceeded the run terminates with `ERR_EXPORT_TOO_LARGE` before the `Result`
frame is written.

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

| Field         | Encoding                 | Notes                                                       |
| ------------- | ------------------------ | ----------------------------------------------------------- |
| `exports`     | `WireValue::Object`      | Contains `default` plus named exports as direct properties. |
| `stdout`      | `List<String>`           | Captured stdout log lines.                                  |
| `stderr`      | `List<String>`           | Captured stderr log lines.                                  |
| `durationMs`  | `f64`                    | Wall-clock runtime duration.                                |
| `cpuTimeMs`   | `f64`                    | Active V8 execution time; bridge waits excluded.            |
| `bridgeCalls` | `List<BridgeCallRecord>` | One record per bridge call attempt, in attempt order.       |

`RunFailurePayload`:

| Field         | Encoding                 |
| ------------- | ------------------------ |
| `error`       | `RunErrorPayload`        |
| `stdout`      | `List<String>`           |
| `stderr`      | `List<String>`           |
| `durationMs`  | `f64`                    |
| `cpuTimeMs`   | `f64`                    |
| `bridgeCalls` | `List<BridgeCallRecord>` |

`BridgeCallRecord` — per-call metadata (names, timing, sizes; never payloads):

| Field           | Encoding | Notes                                                                                                                                                       |
| --------------- | -------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `name`          | `String` | Public call name, resolved by the runtime: plain globals as-is (`fetch`), shims under their public name, host-module import leaves as `<specifier>.<path>`. |
| `startMs`       | `f64`    | Offset from run start (same clock as `durationMs`).                                                                                                         |
| `durationMs`    | `f64`    | Round-trip the sandbox waited; time-until-run-end for calls that never settled.                                                                             |
| `argBytes`      | `u32`    | Serialized call payload size — what `maxBridgeCallBytes` is enforced against.                                                                               |
| `responseBytes` | `u32`    | Serialized response value size; `0` on handler error or unsettled.                                                                                          |
| `ok`            | `bool`   | Handler resolved and the response reached the sandbox.                                                                                                      |
| `blocked`       | `bool`   | Blocked runtime-side (limit, oversized payload, function argument, invalid import handle); never sent.                                                      |

`RunErrorPayload`:

| Field     | Encoding              | Notes                                                 |
| --------- | --------------------- | ----------------------------------------------------- |
| `code`    | `String`              |                                                       |
| `name`    | `String`              |                                                       |
| `message` | `String`              |                                                       |
| `stack`   | `Optional<String>`    | Always absent host → sandbox (BridgeResponse).        |
| `fields`  | `Optional<WireValue>` | Own-enumerable props beyond `name`/`message`/`stack`. |

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
| `ERR_ABORTED`                         | Host aborted the run (sent `Terminate` after its `AbortSignal` fired).      |
| `ERR_MODULE_NOT_FOUND`                | Import specifier not in the resolved import set.                            |
| `ERR_COMPILE`                         | Syntax/module compile error.                                                |
| `ERR_FUNCTION_ARGUMENT_NOT_SUPPORTED` | Function argument attempted to cross the host bridge.                       |
| `ERR_EXPORT_NOT_SERIALIZABLE`         | Export contains unsupported value or cycle.                                 |
| `ERR_EXPORT_TOO_LARGE`                | Encoded exports exceed `limits.maxExportBytes`.                             |
| `ERR_EXPORT_UNRESOLVED_PROMISE`       | Export value is a pending Promise.                                          |
| `ERR_HOST_BRIDGE`                     | Host global/import handler threw or rejected, uncaught by sandbox code.     |
| `ERR_BRIDGE_PAYLOAD_TOO_LARGE`        | Bridge call payload exceeded `limits.maxBridgeCallBytes`.                   |
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
