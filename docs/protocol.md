# iso4 Wire Protocol

Communication between the TypeScript host (`iso4`) and the Rust V8 binary
(`iso4-v8`) happens over a **Unix domain socket** using **length-prefixed
binary frames**.

The direction a frame travels (TS→Rust or Rust→TS) is always known from
context, so message type bytes are scoped per direction — both tables start
at `0x01`. A `0x01` frame from TS means _Authenticate_; a `0x01` frame from
Rust means _BridgeCall_. There is no ambiguity because the receiver always
knows which table applies.

---

## Frame format

Every message, in both directions, uses the same envelope:

```
┌─────────────────────┬──────────────────┬─────────────────────────┐
│  length  (4 bytes)  │  type  (1 byte)  │  payload  (N bytes)     │
│  uint32 big-endian  │  see tables      │  message-specific       │
└─────────────────────┴──────────────────┴─────────────────────────┘
```

- **length** — byte count of `type + payload` (i.e. total frame size minus
  the 4-byte length prefix itself).
- **type** — message type byte from the appropriate direction table.
- **payload** — message-specific bytes. Empty payloads are valid (length = 1,
  only the type byte).

Payload encoding: V8 `ValueSerializer` wire format for structured data
(arguments, results, exports). Raw bytes for stdio chunks. Plain UTF-8 for
log strings.

---

## TS → Rust messages

| Byte   | Name             | Payload                                       | Notes                                                                                                                      |
| ------ | ---------------- | --------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------- |
| `0x01` | `Authenticate`   | `u16` protocol version + UTF-8 token          | First message on every new connection. Rust closes the socket on mismatch.                                                 |
| `0x02` | `Run`            | V8-serialized `RunOptions`                    | Start a sandboxed execution. Rust replies with zero or more `StdioChunk` / `BridgeCall` frames, then exactly one `Result`. |
| `0x03` | `BridgeResponse` | `u32` call-id + V8-serialized result or error | Reply to a `BridgeCall`. Must be sent before the next `BridgeCall` on the same session (calls are sequential in v1).       |
| `0x04` | `Terminate`      | `u32` run-id                                  | Force-stop a running isolate. Rust sends a `Result` with `ERR_TERMINATED` then closes the session.                         |

---

## Rust → TS messages

| Byte   | Name         | Payload                                                   | Notes                                                                                                             |
| ------ | ------------ | --------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------- |
| `0x01` | `BridgeCall` | `u32` call-id + UTF-8 module/fn name + V8-serialized args | Sandbox called `fetch` or a host-module function. TS must reply with `BridgeResponse` before execution continues. |
| `0x02` | `StdioChunk` | `u8` stream (0 = stdout, 1 = stderr) + raw UTF-8 bytes    | Emitted eagerly per `console.*` call. Capped at `limits.maxStdoutBytes` / `limits.maxStderrBytes`.                |
| `0x03` | `Result`     | V8-serialized `RunResult`                                 | Final message for a `Run`. Always sent exactly once, even on error or termination.                                |
| `0x04` | `Log`        | `u8` level (0=debug,1=info,2=warn,3=error) + UTF-8        | Internal runtime diagnostics. TS may forward to its own logger or discard.                                        |

---

## Session lifecycle

Each connection handles exactly one `Run` at a time. The `Runtime` in
TypeScript maintains a pool of connections (one per `maxIsolates` slot),
so concurrent callers each get their own connection and run truly in
parallel inside the Rust process. No message-level multiplexing is needed;
the connection itself is the concurrency unit.

```
TS (one connection slot)              Rust (one isolate thread)
│                                       │
│──── Authenticate ────────────────────▶│  version + token check
│                                       │  (close on mismatch)
│                                       │
│──── Run ─────────────────────────────▶│  spawn / acquire isolate
│                                       │
│◀─── StdioChunk ───────────────────────│  (zero or more, eager)
│                                       │
│◀─── BridgeCall ───────────────────────│  sandbox called fetch / host fn
│──── BridgeResponse ──────────────────▶│  TS resolves and replies
│                                       │
│◀─── Result ───────────────────────────│  run complete (ok or error)
│                                       │
│  (next Run or Terminate on same conn) │
```

Multiple `Run` messages may be sent on the same connection sequentially
(after each `Result` is received). Concurrent runs use separate connection
slots from the pool — there is no intra-connection multiplexing in v1.

---

## Protocol version

The `Authenticate` frame carries a `u16` protocol version. The current
version is **`1`**. Rust rejects connections with a mismatched version by
closing the socket immediately (no error frame — the connection is untrusted
until auth succeeds).

Bump the protocol version whenever the frame format or message set changes
incompatibly. Both `iso4` (TS) and `@iso4/v8-*` (Rust binary) must be
updated together on a version bump.

---

## Error codes (carried in `Result` payload)

| Code                          | Cause                                                                      |
| ----------------------------- | -------------------------------------------------------------------------- |
| `ERR_USER_CODE`               | Uncaught exception in sandbox JS                                           |
| `ERR_MEMORY_LIMIT`            | V8 heap + ArrayBuffer exceeded `limits.memoryMb`                           |
| `ERR_CPU_TIMEOUT`             | Active JS execution exceeded `limits.cpuTimeMs`                            |
| `ERR_WALL_TIMEOUT`            | Total run time exceeded `limits.wallTimeMs`                                |
| `ERR_TERMINATED`              | Host sent `Terminate`                                                      |
| `ERR_MODULE_NOT_FOUND`        | `import` specifier not in static map or resolver                           |
| `ERR_EXPORT_NOT_SERIALIZABLE` | Export value contains a function or Promise                                |
| `ERR_PREFIX_DISPOSED`         | `PrecompiledPrefix` was evicted from the LRU cache                         |
| `ERR_UNDECLARED_BINDING`      | `prefix.run()` passed a name not declared at `precompile()` time           |
| `ERR_FUNCTION_ARGUMENT`       | Host bridge function called with a function argument (not supported in v1) |
| `ERR_HOST_BRIDGE`             | Configured host global/import handler threw or rejected                    |
