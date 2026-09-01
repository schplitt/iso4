//! IPC framing for the iso4 TypeScript host <-> Rust runtime protocol.
//!
//! Start here if you're learning the Rust side of the runtime. This module is
//! intentionally small and mechanical: it only knows how to read and write
//! binary frames, plus parse the simple `Authenticate` payload.
//!
//! The frame envelope is documented in `docs/protocol.md`:
//!
//! ```text
//! ┌─────────────────────┬──────────────────┬─────────────────────────┐
//! │  length  (4 bytes)  │  type  (1 byte)  │  payload  (N bytes)     │
//! │  uint32 big-endian  │  see tables      │  message-specific       │
//! └─────────────────────┴──────────────────┴─────────────────────────┘
//! ```
//!
//! `length` covers `type + payload`, so a frame with no payload has length 1.
//!
//! Keep this module focused on:
//! - reading the frame envelope from any `Read`
//! - writing the frame envelope to any `Write`
//! - parsing the easy non-V8 payloads first (`Authenticate`, later maybe
//!   `Terminate`, `Log`, `StdioChunk`)
//! - leaving V8-serialized payloads (`Run`, `Result`, `BridgeResponse`) as raw
//!   bytes until the runtime is ready to decode them.

use std::io::{self, Read, Write};
use std::mem::ManuallyDrop;
use std::os::unix::io::{FromRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};

/// Current wire protocol version.
///
/// This must stay in sync with `docs/protocol.md` and the TypeScript codec in
/// `packages/iso4-sandbox/src/ipc.ts`.
pub const PROTOCOL_VERSION: u16 = 1;

/// Default maximum frame length in bytes, including the 1-byte message type.
pub const DEFAULT_MAX_FRAME_LENGTH: u32 = 64 * 1024 * 1024;

/// Maximum frame length accepted for the `Authenticate` frame — the one frame
/// read from a peer that has not yet completed the handshake.
///
/// An `AuthenticatePayload` is a `u16` and a probe of a few bytes, so a few
/// dozen bytes in practice. The default ceiling is sized for run payloads
/// carrying prefix source and serialized values, and it is a poor fit here: a
/// frame body is buffered to its declared length, so applying the run ceiling
/// to the handshake lets an unproven peer name a size six orders of magnitude
/// past anything the handshake needs. 4 KiB keeps that buffer to a size the
/// peer could plausibly deliver.
pub const AUTH_MAX_FRAME_LENGTH: u32 = 4 * 1024;

/// Message types sent from the TypeScript host to Rust.
///
/// Byte values match `docs/protocol.md` §2.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TsToRustMessageType {
    /// First frame on a new connection.
    Authenticate = 0x01,
    /// Start a sandboxed execution.
    Run = 0x02,
    /// Compile a prefix into a V8 snapshot.
    Precompile = 0x03,
    /// Run postfix code against a stored snapshot.
    PrefixRun = 0x04,
    /// Release a stored snapshot. Idempotent.
    DisposePrefix = 0x05,
    /// Reply to a `BridgeCall` sent by Rust.
    BridgeResponse = 0x06,
    /// Force-stop a running isolate.
    Terminate = 0x07,
    /// Request a capacity/usage snapshot. Empty payload; answered
    /// with a `StatsResult` frame.
    Stats = 0x08,
    /// One chunk of a streamed host-type body. Only legal inside the credit
    /// window the runtime granted for the stream.
    StreamChunk = 0x09,
    /// End of a streamed body: clean EOF, or a source failure carrying a
    /// message the pending sandbox read rejects with.
    StreamEnd = 0x0A,
}

/// Message types sent from Rust to the TypeScript host.
///
/// Byte values match `docs/protocol.md` §2.2. There is no `StdioChunk` in the
/// real protocol — stdout/stderr are captured by Rust and included inside the
/// `Result` payload at the end of the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum RustToTsMessageType {
    /// Sandbox called a configured host global/function or host import.
    BridgeCall = 0x01,
    /// Final run result. Payload is a `RunCompletionPayload`.
    Result = 0x02,
    /// Result of a `Precompile` request.
    PrecompileResult = 0x03,
    /// Internal runtime diagnostic log (not sandbox stdout/stderr).
    Log = 0x04,
    /// Handshake acknowledgement — the first frame sent on a new connection,
    /// answering `Authenticate`.
    Hello = 0x05,
    /// Capacity/usage snapshot answering a `Stats` request.
    StatsResult = 0x06,
    /// Final frame of a run whose Result reported pending background work:
    /// carries the `waitUntil` epilogue's outcome and telemetry, and
    /// releases the run's connection slot.
    RunComplete = 0x07,
    /// Grant of streaming-body credit: the sandbox consumed bytes off a
    /// streamed body, the host may have that many more bytes in flight.
    StreamPull = 0x08,
    /// The sandbox cancelled a streamed body (`reader.cancel()`, instance
    /// teardown); the host must stop pumping and release the source.
    StreamCancel = 0x09,
}

/// Handshake status reported on a `Hello` frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum HelloStatus {
    /// Handshake accepted; the connection is live.
    Ok = 0,
    /// The host speaks a different `PROTOCOL_VERSION`.
    ProtocolVersionMismatch = 1,
    /// The host writes a V8 serialization format this binary cannot read.
    V8FormatMismatch = 2,
}

/// A raw wire frame after the outer envelope has been decoded.
///
/// At this layer we do not yet interpret most payloads. We just split the
/// frame into:
/// - the 1-byte message type
/// - the remaining payload bytes
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// Raw type byte from the wire.
    pub message_type: u8,
    /// Message-specific bytes after the type byte.
    pub payload: Vec<u8>,
}

/// A frame whose message type has been validated against one direction table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypedFrame<MessageType> {
    pub message_type: MessageType,
    pub payload: Vec<u8>,
}

pub type TsToRustFrame = TypedFrame<TsToRustMessageType>;
pub type RustToTsFrame = TypedFrame<RustToTsMessageType>;

/// Parsed contents of an `Authenticate` payload.
///
/// Payload layout:
/// - `u16` protocol version (big-endian)
/// - `u32` probe length + probe bytes
/// - `u32` token length + token bytes (exactly 16)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthenticatePayload {
    pub protocol_version: u16,
    /// The host's V8 serialization probe — a serialized `null` whose second
    /// byte is the format version Node writes. Compared against this binary's
    /// own write version during the handshake.
    pub probe: Vec<u8>,
    /// Random per-sandbox descriptor token (`docs/protocol.md` §5.1). Host-
    /// emitted host-type descriptors are stamped with a brand key derived from
    /// it; the runtime rehydrates only descriptors carrying that key. Exactly
    /// [`crate::webcodec::DESCRIPTOR_TOKEN_LEN`] bytes; the host sends the
    /// same token on every connection of one sandbox.
    pub descriptor_token: Vec<u8>,
}

/// Read a single raw frame from `reader` using the default frame-length cap.
pub fn read_frame(reader: &mut impl Read) -> io::Result<Frame> {
    read_frame_with_limit(reader, DEFAULT_MAX_FRAME_LENGTH)
}

/// Read a single raw frame from `reader` with an explicit frame-length cap.
pub fn read_frame_with_limit(reader: &mut impl Read, max_frame_length: u32) -> io::Result<Frame> {
    let mut length_prefix = [0; 4];
    reader.read_exact(&mut length_prefix)?;
    let length = u32::from_be_bytes(length_prefix);
    if length == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame length cannot be zero",
        ));
    }
    if length > max_frame_length {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame length {length} exceeds max frame length {max_frame_length}"),
        ));
    }
    let mut buffer = vec![0; length as usize];
    reader.read_exact(&mut buffer)?;

    let type_byte = buffer[0];
    let payload = buffer[1..].to_vec();

    Ok(Frame {
        message_type: type_byte,
        payload,
    })
}

/// Read a single TS->Rust frame and validate its message type byte.
pub fn read_ts_to_rust_frame(reader: &mut impl Read) -> io::Result<TsToRustFrame> {
    let frame = read_frame(reader)?;
    Ok(TypedFrame {
        message_type: parse_ts_to_rust_message_type(frame.message_type)?,
        payload: frame.payload,
    })
}

/// Read a single Rust->TS frame and validate its message type byte.
pub fn read_rust_to_ts_frame(reader: &mut impl Read) -> io::Result<RustToTsFrame> {
    let frame = read_frame(reader)?;
    Ok(TypedFrame {
        message_type: parse_rust_to_ts_message_type(frame.message_type)?,
        payload: frame.payload,
    })
}

/// Read a single TS->Rust frame with an explicit frame-length cap.
/// Use this when per-run limits are available (e.g. when reading BridgeResponse
/// frames inside the poll loop where `memory_mb` and `max_bridge_response_bytes`
/// are known).
pub fn read_ts_to_rust_frame_with_limit(
    reader: &mut impl Read,
    max_frame_length: u32,
) -> io::Result<TsToRustFrame> {
    let frame = read_frame_with_limit(reader, max_frame_length)?;
    Ok(TypedFrame {
        message_type: parse_ts_to_rust_message_type(frame.message_type)?,
        payload: frame.payload,
    })
}

/// Write a single raw frame to `writer` using the default frame-length cap.
pub fn write_frame(writer: &mut impl Write, message_type: u8, payload: &[u8]) -> io::Result<()> {
    write_frame_with_limit(writer, message_type, payload, DEFAULT_MAX_FRAME_LENGTH)
}

/// Write a single raw frame to `writer` with an explicit frame-length cap.
pub fn write_frame_with_limit(
    writer: &mut impl Write,
    message_type: u8,
    payload: &[u8],
    max_frame_length: u32,
) -> io::Result<()> {
    let payload_length = u32::try_from(payload.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "payload too large"))?;
    let length = payload_length
        .checked_add(1)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "payload too large"))?;
    if length > max_frame_length {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("frame length {length} exceeds max frame length {max_frame_length}"),
        ));
    }

    writer.write_all(&length.to_be_bytes())?;
    writer.write_all(&[message_type])?;
    writer.write_all(payload)?;
    Ok(())
}

/// Write a Rust->TS frame whose payload is already in two pieces.
///
/// Same bytes on the wire as `write_rust_to_ts_frame(writer, ty, [head,
/// tail].concat())`, without building that concatenation. The writer already
/// emits the length and the type as their own calls, so this adds one more
/// rather than changing how a frame is written. It exists for `BridgeCall`,
/// whose payload is a small header in front of an argument blob that can be
/// large and that nothing has copied yet.
pub fn write_rust_to_ts_frame_parts(
    writer: &mut impl Write,
    message_type: RustToTsMessageType,
    head: &[u8],
    tail: &[u8],
) -> io::Result<()> {
    let payload_length = head
        .len()
        .checked_add(tail.len())
        .and_then(|total| u32::try_from(total).ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "payload too large"))?;
    let length = payload_length
        .checked_add(1)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "payload too large"))?;
    if length > DEFAULT_MAX_FRAME_LENGTH {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("frame length {length} exceeds max frame length {DEFAULT_MAX_FRAME_LENGTH}"),
        ));
    }

    // One gather-write per frame: with concurrent runs sharing a connection,
    // every frame is written under the connection's writer lock, so the lock
    // must hold ONE syscall, not four. The 5-byte envelope goes on the
    // stack; the payload pieces are never copied.
    let mut envelope = [0u8; 5];
    envelope[..4].copy_from_slice(&length.to_be_bytes());
    envelope[4] = message_type as u8;
    let mut bufs = [
        io::IoSlice::new(&envelope),
        io::IoSlice::new(head),
        io::IoSlice::new(tail),
    ];
    write_all_vectored(writer, &mut bufs)
}

/// `write_all` over a set of buffers via gather I/O, advancing across
/// partial writes (std's `write_all_vectored` is still unstable).
fn write_all_vectored(writer: &mut impl Write, mut bufs: &mut [io::IoSlice<'_>]) -> io::Result<()> {
    // Skip leading empty slices so `write_vectored` never sees a fully
    // consumed set (advance_slices panics past the total length).
    io::IoSlice::advance_slices(&mut bufs, 0);
    while bufs.iter().any(|b| !b.is_empty()) {
        match writer.write_vectored(bufs) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "failed to write whole frame",
                ));
            }
            Ok(n) => io::IoSlice::advance_slices(&mut bufs, n),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Write a single TS->Rust frame using a validated message type.
pub fn write_ts_to_rust_frame(
    writer: &mut impl Write,
    message_type: TsToRustMessageType,
    payload: &[u8],
) -> io::Result<()> {
    write_frame(writer, message_type as u8, payload)
}

/// Write a single Rust->TS frame using a validated message type.
pub fn write_rust_to_ts_frame(
    writer: &mut impl Write,
    message_type: RustToTsMessageType,
    payload: &[u8],
) -> io::Result<()> {
    write_frame(writer, message_type as u8, payload)
}

// ── Outbound frame sink ──────────────────────────────────────────────────────

/// Where a run's outbound frames (BridgeCall, early Result, stream credit,
/// RunComplete, …) go.
///
/// Two modes share the session protocol but not the socket discipline:
///
/// - `Fd`: the raw session-socket fd, written directly. The direct-API mode —
///   one run per socket by construction, so the writing thread is the fd's
///   only user (the `ManuallyDrop` discipline the bridge callbacks have always
///   used: the fd stays owned by whoever opened the socket).
/// - `Shared`: one serialized per-connection writer. Session runs share their
///   connection with other runs, so every outbound frame takes the lock for
///   exactly one frame write — frames from concurrent runs interleave on the
///   stream but never mid-frame.
#[derive(Clone)]
pub enum FrameSink {
    Fd(RawFd),
    Shared(Arc<Mutex<UnixStream>>),
}

impl FrameSink {
    /// Write one frame whose payload is already in two pieces (the
    /// `BridgeCall` shape: small header + large blob nothing has copied).
    /// `write(ty, payload)` is the one-piece special case.
    pub fn write_parts(
        &self,
        message_type: RustToTsMessageType,
        head: &[u8],
        tail: &[u8],
    ) -> io::Result<()> {
        match self {
            FrameSink::Fd(fd) => {
                // SAFETY: the fd is the live session socket owned by the
                // caller of the run; ManuallyDrop prevents closing it here.
                let mut stream = ManuallyDrop::new(unsafe { UnixStream::from_raw_fd(*fd) });
                write_rust_to_ts_frame_parts(&mut *stream, message_type, head, tail)
            }
            FrameSink::Shared(writer) => {
                let mut stream = writer.lock().unwrap_or_else(|p| p.into_inner());
                write_rust_to_ts_frame_parts(&mut *stream, message_type, head, tail)
            }
        }
    }

    pub fn write(&self, message_type: RustToTsMessageType, payload: &[u8]) -> io::Result<()> {
        self.write_parts(message_type, payload, &[])
    }
}

/// Parse the payload bytes of an `Authenticate` frame per
/// `docs/protocol.md` §5.1.
pub fn parse_authenticate_payload(payload: &[u8]) -> io::Result<AuthenticatePayload> {
    if payload.len() < 6 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "payload too short for Authenticate",
        ));
    }
    let protocol_version = u16::from_be_bytes([payload[0], payload[1]]);
    let probe_len = u32::from_be_bytes([payload[2], payload[3], payload[4], payload[5]]) as usize;
    let probe_end = 6usize
        .checked_add(probe_len)
        .filter(|end| *end + 4 <= payload.len())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "Authenticate payload truncated (probe)",
            )
        })?;
    let probe = payload[6..probe_end].to_vec();

    let token_len = u32::from_be_bytes(
        payload[probe_end..probe_end + 4]
            .try_into()
            .expect("bounds checked above"),
    ) as usize;
    if token_len != crate::webcodec::DESCRIPTOR_TOKEN_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "descriptor token must be exactly {} bytes, got {token_len}",
                crate::webcodec::DESCRIPTOR_TOKEN_LEN
            ),
        ));
    }
    let token_end = probe_end + 4 + token_len;
    if token_end > payload.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Authenticate payload truncated (descriptor token)",
        ));
    }
    if token_end != payload.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Authenticate payload has trailing bytes",
        ));
    }
    Ok(AuthenticatePayload {
        protocol_version,
        probe,
        descriptor_token: payload[probe_end + 4..token_end].to_vec(),
    })
}

/// Encode an `Authenticate` payload from structured fields — the inverse of
/// `parse_authenticate_payload`, used by tests.
pub fn encode_authenticate_payload(auth: &AuthenticatePayload) -> Vec<u8> {
    let mut payload =
        Vec::with_capacity(6 + auth.probe.len() + 4 + auth.descriptor_token.len());
    payload.extend_from_slice(&auth.protocol_version.to_be_bytes());
    payload.extend_from_slice(&(auth.probe.len() as u32).to_be_bytes());
    payload.extend_from_slice(&auth.probe);
    payload.extend_from_slice(&(auth.descriptor_token.len() as u32).to_be_bytes());
    payload.extend_from_slice(&auth.descriptor_token);
    payload
}

/// Encode a `Hello` payload — the handshake acknowledgement.
///
/// Layout: `u8 status`, `u32 probeLength + probe bytes`, `String message`
/// (empty when `status = Ok`). See `docs/protocol.md` §5.1.
pub fn encode_hello_payload(status: HelloStatus, probe: &[u8], message: &str) -> Vec<u8> {
    let mut payload = Vec::with_capacity(1 + 4 + probe.len() + 4 + message.len());
    payload.push(status as u8);
    payload.extend_from_slice(&(probe.len() as u32).to_be_bytes());
    payload.extend_from_slice(probe);
    payload.extend_from_slice(&(message.len() as u32).to_be_bytes());
    payload.extend_from_slice(message.as_bytes());
    payload
}

// ── RunPayload types ─────────────────────────────────────────────────────────

/// Default resource limits. The runtime owns its safety posture: the TS client
/// sends only the limits the caller explicitly set (each wire field is
/// `Optional<u32>`), and any absent field is filled from these constants. An
/// explicit `0` on the wire is preserved as-is and disables that limit.
///
/// The public `ResourceLimits` jsdoc in `packages/iso4-sandbox/src/types.ts`
/// documents these same numbers, pointing here as the source of truth.
pub const DEFAULT_MEMORY_MB: u32 = 128;
pub const DEFAULT_CPU_TIME_MS: u32 = 5_000;
pub const DEFAULT_WALL_TIME_MS: u32 = 30_000;
pub const DEFAULT_MAX_EXPORT_BYTES: u32 = 16 * 1024 * 1024;
pub const DEFAULT_MAX_STDOUT_BYTES: u32 = 1024 * 1024;
pub const DEFAULT_MAX_STDERR_BYTES: u32 = 1024 * 1024;
pub const DEFAULT_MAX_BRIDGE_CALL_BYTES: u32 = 16 * 1024 * 1024;
pub const DEFAULT_MAX_BRIDGE_CALLS: u32 = 10;
/// Grace budget for `waitUntil` background work after the Result frame,
/// Cloudflare's number. Zero disables the epilogue entirely.
pub const DEFAULT_GRACE_MS: u32 = 30_000;

/// Streaming bodies: hard cap on one `StreamChunk`'s byte length. The host
/// splits larger reads; the runtime rejects a violation as malformed.
pub const STREAM_CHUNK_MAX_BYTES: u32 = 64 * 1024;
/// Streaming bodies: the credit window — how many un-consumed bytes may be in
/// flight per stream. Granted implicitly at hydration; replenished by
/// `StreamPull` as the sandbox consumes. Bounds runtime-side buffering.
pub const STREAM_CREDIT_WINDOW_BYTES: u32 = 256 * 1024;

/// Resource limits applied to a `Run` request, with runtime defaults already
/// resolved: each field is the caller's explicit value or, when the caller left
/// it unset on the wire, the corresponding `DEFAULT_*` constant. A field of
/// zero means the limit is explicitly disabled (no cap).
#[derive(Debug, Clone)]
pub struct ResourceLimits {
    pub memory_mb: u32,
    pub cpu_time_ms: u32,
    pub wall_time_ms: u32,
    pub max_export_bytes: u32,
    pub max_stdout_bytes: u32,
    pub max_stderr_bytes: u32,
    /// Maximum bytes the sandbox may send as arguments in a single bridge call
    /// (sandbox → host). Zero means no per-call cap.
    pub max_bridge_call_bytes: u32,
    /// Maximum number of bridge calls (globals + host imports combined) allowed
    /// per run. Zero means no limit.
    pub max_bridge_calls: u32,
    /// Wall budget for `waitUntil` background work after the Result frame
    ///. Zero disables the epilogue: registered work dies at settle.
    pub grace_ms: u32,
}

impl Default for ResourceLimits {
    /// The runtime defaults — the posture applied when the client sends no
    /// explicit limits at all.
    fn default() -> Self {
        Self {
            memory_mb: DEFAULT_MEMORY_MB,
            cpu_time_ms: DEFAULT_CPU_TIME_MS,
            wall_time_ms: DEFAULT_WALL_TIME_MS,
            max_export_bytes: DEFAULT_MAX_EXPORT_BYTES,
            max_stdout_bytes: DEFAULT_MAX_STDOUT_BYTES,
            max_stderr_bytes: DEFAULT_MAX_STDERR_BYTES,
            max_bridge_call_bytes: DEFAULT_MAX_BRIDGE_CALL_BYTES,
            max_bridge_calls: DEFAULT_MAX_BRIDGE_CALLS,
            grace_ms: DEFAULT_GRACE_MS,
        }
    }
}

/// A host global the sandbox is allowed to reference, tagged by how the
/// runtime installs it natively. Mirrors `GlobalDefPayload` on the TS side
/// (`ipc.ts`); see `docs/protocol.md` §5.2.
/// Every variant carries `enumerable`: whether the installed public name
/// shows up in `for...in` / `Object.keys`. Host shorthands always send
/// `true`; the object global forms may opt out. Runtime-internal names
/// (`__iso4_*`) install non-enumerable regardless.
#[derive(Debug, Clone)]
pub enum HostGlobalDef {
    /// A plain host function — installed as a bridge stub under `name`.
    Bridge { name: String, enumerable: bool },
    /// A JS expression the runtime evaluates as its own script and sets on
    /// `globalThis[name]`.
    StringExpr {
        name: String,
        expr: String,
        enumerable: bool,
    },
    /// A constant carried as a V8 serialization blob, materialised natively.
    Data {
        name: String,
        blob: Vec<u8>,
        enumerable: bool,
    },
    /// A bridge handler (installed as a stub under `handler_name`) plus a shim
    /// expression the runtime wraps and sets on `globalThis[name]`.
    Shim {
        name: String,
        shim: String,
        handler_name: String,
        enumerable: bool,
    },
}

impl HostGlobalDef {
    /// The wire-level bridge stub name to install for this def, if any. Plain
    /// bridge globals install under their own name; shims install a hidden
    /// handler stub under `handler_name`. String/data globals need no stub.
    ///
    /// This is also the set of names that a `PrefixRun` may re-install and that
    /// the `ERR_UNDECLARED_BINDING` check validates against.
    pub fn bridge_stub_name(&self) -> Option<&str> {
        match self {
            HostGlobalDef::Bridge { name, .. } => Some(name),
            HostGlobalDef::Shim { handler_name, .. } => Some(handler_name),
            HostGlobalDef::StringExpr { .. } | HostGlobalDef::Data { .. } => None,
        }
    }

    /// Whether the installed public name is enumerable.
    pub fn enumerable(&self) -> bool {
        match self {
            HostGlobalDef::Bridge { enumerable, .. }
            | HostGlobalDef::StringExpr { enumerable, .. }
            | HostGlobalDef::Data { enumerable, .. }
            | HostGlobalDef::Shim { enumerable, .. } => *enumerable,
        }
    }

    /// A convenience constructor for tests: a plain bridge global.
    #[cfg(test)]
    pub fn bridge(name: &str) -> Self {
        HostGlobalDef::Bridge {
            name: name.to_string(),
            enumerable: true,
        }
    }
}

/// One node in a host-module shape tree. The tree is plain data — the client
/// never generates sandbox source from it; the runtime builds the module
/// natively (see `v8.rs`). Mirrors `HostModuleNodePayload` on the TS side.
#[derive(Debug, Clone)]
pub enum HostModuleNode {
    /// A host function leaf. The runtime assigns it a handle ID (tree-walk
    /// order over the declared bindings) and installs an async trampoline
    /// that dispatches through the reserved `__iso4_call` bridge stub.
    Function,
    /// A constant data leaf carried as a V8 serialization blob, materialised
    /// natively via `blob::deserialize_value`.
    Data(Vec<u8>),
    /// A nested object of named children (may mix functions and data).
    Object(Vec<(String, HostModuleNode)>),
}

/// The two flavors of import module per DESIGN.md §4.3, now discriminated on
/// the wire instead of being lowered to source text by the client.
#[derive(Debug, Clone)]
pub enum ImportModule {
    /// Host-provided ESM source, compiled verbatim inside the isolate.
    Source(String),
    /// A host-module shape: named top-level exports, each a tree of function
    /// leaves and data leaves. The runtime builds the module from this data.
    Host(Vec<(String, HostModuleNode)>),
}

/// One entry in the `imports` list of a `Run` / `Precompile` request.
#[derive(Debug, Clone)]
pub struct ImportBinding {
    pub specifier: String,
    pub module: ImportModule,
}

/// One host-import function-leaf rebinding requested by a `PrefixRun`. Only
/// the location crosses the wire — the replacement handler stays on the TS
/// side (bridge dispatch is name-addressed). The runtime validates each entry
/// against the shape declared at `Precompile` and rejects anything else with
/// `ERR_UNDECLARED_BINDING`.
#[derive(Debug, Clone)]
pub struct ImportRebind {
    pub specifier: String,
    /// Dot-joined path of the function leaf inside the host module
    /// (e.g. `"someObj.someMethod"`).
    pub path: String,
}

/// A host → sandbox function call requested by a `Run` / `PrefixRun` frame
///. `export_path` addresses a callable relative to the module's exports
/// (`"named"`, `"default.fetch"`); `args_blob` is **one** V8 serialization
/// blob holding an array of arguments — the same convention as `BridgeCall`
/// args, identity between arguments preserved.
#[derive(Debug)]
pub struct CallSpec {
    pub export_path: String,
    pub args_blob: Vec<u8>,
}

/// Fully parsed `Run` frame payload per `docs/protocol.md` §5.2.
#[derive(Debug)]
pub struct RunPayload {
    pub run_id: u32,
    pub code: String,
    pub filename: Option<String>,
    pub limits: ResourceLimits,
    pub globals: Vec<HostGlobalDef>,
    pub imports: Vec<ImportBinding>,
    /// When present, the result is the called function's return value instead
    /// of the exports. Resolved against the freshly evaluated module.
    pub call: Option<CallSpec>,
}

// ── Payload reader ────────────────────────────────────────────────────────────

/// Minimal cursor used for parsing fixed-format payloads.
/// Not exported — internal to this module.
struct PayloadReader<'a> {
    data: &'a [u8],
    offset: usize,
}

impl<'a> PayloadReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, offset: 0 }
    }

    fn remaining(&self) -> usize {
        self.data.len() - self.offset
    }

    fn read_u8(&mut self) -> io::Result<u8> {
        if self.remaining() < 1 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "payload too short reading u8",
            ));
        }
        let b = self.data[self.offset];
        self.offset += 1;
        Ok(b)
    }

    fn read_u32(&mut self) -> io::Result<u32> {
        if self.remaining() < 4 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "payload too short reading u32",
            ));
        }
        let n = u32::from_be_bytes(self.data[self.offset..self.offset + 4].try_into().unwrap());
        self.offset += 4;
        Ok(n)
    }

    /// Strict wire bool per §3: `0 = false`, `1 = true`, anything else is
    /// malformed.
    fn read_bool(&mut self) -> io::Result<bool> {
        match self.read_u8()? {
            0 => Ok(false),
            1 => Ok(true),
            other => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid boolean byte: {other:#04x}"),
            )),
        }
    }

    /// Read the `u32` entry count that introduces a `List<…>` block and return
    /// it together with a vector already sized for that many entries.
    ///
    /// A count is a size, and a size read off the wire is a size the sender
    /// chose, so it gets two checks before any memory is asked for.
    ///
    /// First, it has to be a count this payload could actually back. Every
    /// list entry in this format costs at least one byte — a tag byte or a
    /// four-byte length prefix — so a count larger than `remaining()`
    /// describes a payload that cannot exist, and the sender is either broken
    /// or not speaking this protocol. That puts counts on the same footing as
    /// the length checks in `read_string` and `read_bytes` below, which have
    /// always validated before touching memory.
    ///
    /// Second, the reservation itself has to be allowed to fail. Sizing up
    /// front is the right shape — these lists are read once per run and one
    /// allocation beats growing through several — but `Vec::with_capacity`
    /// offers no way to decline: a request the allocator cannot serve reaches
    /// `handle_alloc_error`, which aborts, and `[profile.release]` sets
    /// `panic = "abort"`, so there is no unwinding to contain it either. A
    /// whole child process ending takes every run on every connection with
    /// it. `try_reserve_exact` is the same single allocation with an error
    /// path, so a count this machine cannot honour ends one connection with a
    /// log line instead.
    fn read_list<T>(&mut self, what: &str) -> io::Result<(usize, Vec<T>)> {
        let count = self.read_u32()? as usize;
        if count > self.remaining() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{what} count {count} exceeds the {} bytes remaining in the payload",
                    self.remaining()
                ),
            ));
        }
        let mut items = Vec::new();
        items.try_reserve_exact(count).map_err(|_| {
            io::Error::new(
                io::ErrorKind::OutOfMemory,
                format!("cannot reserve room for {count} {what} entries"),
            )
        })?;
        Ok((count, items))
    }

    fn read_string(&mut self) -> io::Result<String> {
        let len = self.read_u32()? as usize;
        if self.remaining() < len {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "payload too short reading string body",
            ));
        }
        let bytes = &self.data[self.offset..self.offset + len];
        let s = String::from_utf8(bytes.to_vec()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "string payload is not valid UTF-8",
            )
        })?;
        self.offset += len;
        Ok(s)
    }

    fn read_bytes(&mut self, len: usize) -> io::Result<Vec<u8>> {
        if self.remaining() < len {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "payload too short reading byte slice",
            ));
        }
        let out = self.data[self.offset..self.offset + len].to_vec();
        self.offset += len;
        Ok(out)
    }

    fn read_optional_string(&mut self) -> io::Result<Option<String>> {
        match self.read_u8()? {
            0 => Ok(None),
            1 => Ok(Some(self.read_string()?)),
            b => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid optional presence byte: {b:#04x}"),
            )),
        }
    }

    fn read_optional_u32(&mut self) -> io::Result<Option<u32>> {
        match self.read_u8()? {
            0 => Ok(None),
            1 => Ok(Some(self.read_u32()?)),
            b => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid optional presence byte: {b:#04x}"),
            )),
        }
    }

    /// Read a `ResourceLimits` block: eight `Optional<u32>` fields. Any field
    /// the client left absent is filled from the runtime default, so the
    /// returned struct is fully resolved. An explicit `0` is preserved (it
    /// disables that limit). See `docs/protocol.md` §5.2.
    fn read_resource_limits(&mut self) -> io::Result<ResourceLimits> {
        Ok(ResourceLimits {
            memory_mb: self.read_optional_u32()?.unwrap_or(DEFAULT_MEMORY_MB),
            cpu_time_ms: self.read_optional_u32()?.unwrap_or(DEFAULT_CPU_TIME_MS),
            wall_time_ms: self.read_optional_u32()?.unwrap_or(DEFAULT_WALL_TIME_MS),
            max_export_bytes: self
                .read_optional_u32()?
                .unwrap_or(DEFAULT_MAX_EXPORT_BYTES),
            max_stdout_bytes: self
                .read_optional_u32()?
                .unwrap_or(DEFAULT_MAX_STDOUT_BYTES),
            max_stderr_bytes: self
                .read_optional_u32()?
                .unwrap_or(DEFAULT_MAX_STDERR_BYTES),
            max_bridge_call_bytes: self
                .read_optional_u32()?
                .unwrap_or(DEFAULT_MAX_BRIDGE_CALL_BYTES),
            max_bridge_calls: self
                .read_optional_u32()?
                .unwrap_or(DEFAULT_MAX_BRIDGE_CALLS),
            grace_ms: self.read_optional_u32()?.unwrap_or(DEFAULT_GRACE_MS),
        })
    }

    /// Read a value slot: `u32 byteLength` + V8 serialization blob. The bytes
    /// are carried as-is; materialising them needs an isolate, which this
    /// layer deliberately does not have (see `blob::deserialize_value`).
    fn read_value_blob(&mut self) -> io::Result<Vec<u8>> {
        let len = self.read_u32()? as usize;
        self.read_bytes(len)
    }

    /// Read the `List<GlobalDef>` block: a `u32` count followed by one tagged
    /// entry per global. Mirrors `writeGlobalDefs` in the TS codec (`ipc.ts`)
    /// and `docs/protocol.md` §5.2.
    fn read_global_defs(&mut self) -> io::Result<Vec<HostGlobalDef>> {
        let (count, mut defs) = self.read_list("global def")?;
        for _ in 0..count {
            let kind = self.read_u8()?;
            let name = self.read_string()?;
            let enumerable = self.read_bool()?;
            let def = match kind {
                0 => HostGlobalDef::Bridge { name, enumerable },
                1 => HostGlobalDef::StringExpr {
                    name,
                    expr: self.read_string()?,
                    enumerable,
                },
                2 => HostGlobalDef::Data {
                    name,
                    blob: self.read_value_blob()?,
                    enumerable,
                },
                3 => HostGlobalDef::Shim {
                    name,
                    shim: self.read_string()?,
                    handler_name: self.read_string()?,
                    enumerable,
                },
                other => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("unknown global def kind: {other:#04x}"),
                    ))
                }
            };
            defs.push(def);
        }
        Ok(defs)
    }

    /// Read one host-module shape node. Tags mirror `writeHostModuleNode` in
    /// the TS codec (`ipc.ts`) and `docs/protocol.md` §5.2:
    /// `0 = function`, `1 = data (value blob)`, `2 = object`.
    fn read_host_module_node(&mut self) -> io::Result<HostModuleNode> {
        match self.read_u8()? {
            0 => Ok(HostModuleNode::Function),
            1 => Ok(HostModuleNode::Data(self.read_value_blob()?)),
            2 => {
                let (count, mut entries) = self.read_list("host-module object entry")?;
                for _ in 0..count {
                    let key = self.read_string()?;
                    let node = self.read_host_module_node()?;
                    entries.push((key, node));
                }
                Ok(HostModuleNode::Object(entries))
            }
            other => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown host-module node tag: {other:#04x}"),
            )),
        }
    }

    /// Read the `List<ImportBinding>` block of a `Run` / `Precompile` payload.
    fn read_import_bindings(&mut self) -> io::Result<Vec<ImportBinding>> {
        let (count, mut imports) = self.read_list("import binding")?;
        for _ in 0..count {
            let specifier = self.read_string()?;
            let module = match self.read_u8()? {
                0 => ImportModule::Source(self.read_string()?),
                1 => {
                    let (export_count, mut exports) = self.read_list("host-module export")?;
                    for _ in 0..export_count {
                        let name = self.read_string()?;
                        let node = self.read_host_module_node()?;
                        exports.push((name, node));
                    }
                    ImportModule::Host(exports)
                }
                other => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("unknown import binding kind: {other:#04x}"),
                    ))
                }
            };
            imports.push(ImportBinding { specifier, module });
        }
        Ok(imports)
    }

    /// Read the `List<ImportRebind>` block of a `PrefixRun` payload.
    fn read_import_rebinds(&mut self) -> io::Result<Vec<ImportRebind>> {
        let (count, mut rebinds) = self.read_list("import rebind")?;
        for _ in 0..count {
            let specifier = self.read_string()?;
            let path = self.read_string()?;
            rebinds.push(ImportRebind { specifier, path });
        }
        Ok(rebinds)
    }

    /// Read an `Optional<CallSpec>` slot: a presence byte, then
    /// `String exportPath` + a value blob holding the argument array.
    fn read_optional_call(&mut self) -> io::Result<Option<CallSpec>> {
        match self.read_u8()? {
            0 => Ok(None),
            1 => Ok(Some(CallSpec {
                export_path: self.read_string()?,
                args_blob: self.read_value_blob()?,
            })),
            b => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid optional presence byte: {b:#04x}"),
            )),
        }
    }

    fn assert_done(&self) -> io::Result<()> {
        if self.remaining() != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} trailing bytes in payload", self.remaining()),
            ));
        }
        Ok(())
    }
}

// ── Payload parsers ──────────────────────────────────────────────────────────────

/// Parse the shared code + filename + limits + globals fields that appear in
/// `RunPayload`, `PrecompilePayload`, and `PrefixRunPayload`. The `imports`
/// block that follows differs per message type (`ImportBinding` declarations
/// for Run/Precompile, `ImportRebind` entries for PrefixRun) and is read by
/// the individual parsers.
fn parse_code_fields(
    r: &mut PayloadReader,
) -> io::Result<(String, Option<String>, ResourceLimits, Vec<HostGlobalDef>)> {
    let code = r.read_string()?;
    let filename = r.read_optional_string()?;
    let limits = r.read_resource_limits()?;
    let globals = r.read_global_defs()?;
    Ok((code, filename, limits, globals))
}

/// Parse the payload bytes of a `Run` frame per `docs/protocol.md` §5.2.
pub fn parse_run_payload(payload: &[u8]) -> io::Result<RunPayload> {
    let mut r = PayloadReader::new(payload);
    let run_id = r.read_u32()?;
    let (code, filename, limits, globals) = parse_code_fields(&mut r)?;
    let imports = r.read_import_bindings()?;
    let call = r.read_optional_call()?;
    r.assert_done()?;
    Ok(RunPayload {
        run_id,
        code,
        filename,
        limits,
        globals,
        imports,
        call,
    })
}

/// Fully parsed `Precompile` frame payload per `docs/protocol.md` §5.2.
/// Same fields as `RunPayload` but without `run_id`.
#[derive(Debug)]
pub struct PrecompilePayload {
    pub code: String,
    pub filename: Option<String>,
    pub limits: ResourceLimits,
    pub globals: Vec<HostGlobalDef>,
    pub imports: Vec<ImportBinding>,
}

/// Parse the payload bytes of a `Precompile` frame per `docs/protocol.md` §5.2.
pub fn parse_precompile_payload(payload: &[u8]) -> io::Result<PrecompilePayload> {
    let mut r = PayloadReader::new(payload);
    let (code, filename, limits, globals) = parse_code_fields(&mut r)?;
    let imports = r.read_import_bindings()?;
    r.assert_done()?;
    Ok(PrecompilePayload {
        code,
        filename,
        limits,
        globals,
        imports,
    })
}

/// Fully parsed `PrefixRun` frame payload per `docs/protocol.md` §5.2.
///
/// Unlike `Run`/`Precompile`, the imports block carries `ImportRebind`
/// entries — the declared module shapes are frozen with the snapshot and
/// re-used from the prefix store; only host function leaves may be re-pointed
/// at new TS handlers per run.
#[derive(Debug)]
pub struct PrefixRunPayload {
    pub run_id: u32,
    pub prefix_id: String,
    /// The postfix module source. A frame carries a postfix *or* a call —
    /// exactly one; the parser enforces this.
    pub code: Option<String>,
    pub filename: Option<String>,
    pub limits: ResourceLimits,
    pub globals: Vec<HostGlobalDef>,
    pub import_rebinds: Vec<ImportRebind>,
    /// When present, the run calls into the prefix module's exports instead of
    /// evaluating a postfix; the result is the function's return value.
    pub call: Option<CallSpec>,
}

/// Parse the payload bytes of a `PrefixRun` frame per `docs/protocol.md` §5.2.
pub fn parse_prefix_run_payload(payload: &[u8]) -> io::Result<PrefixRunPayload> {
    let mut r = PayloadReader::new(payload);
    let run_id = r.read_u32()?;
    let prefix_id = r.read_string()?;
    let code = r.read_optional_string()?;
    let filename = r.read_optional_string()?;
    let limits = r.read_resource_limits()?;
    let globals = r.read_global_defs()?;
    let import_rebinds = r.read_import_rebinds()?;
    let call = r.read_optional_call()?;
    r.assert_done()?;
    if code.is_some() == call.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "PrefixRun must carry exactly one of code or call",
        ));
    }
    Ok(PrefixRunPayload {
        run_id,
        prefix_id,
        code,
        filename,
        limits,
        globals,
        import_rebinds,
        call,
    })
}

/// Parse the payload bytes of a `DisposePrefix` frame.
/// Payload is a single `PrefixId` (a length-prefixed UTF-8 string).
pub fn parse_dispose_prefix_payload(payload: &[u8]) -> io::Result<String> {
    let mut r = PayloadReader::new(payload);
    let id = r.read_string()?;
    r.assert_done()?;
    Ok(id)
}

/// Parse the payload bytes of a `Terminate` frame.
/// Payload is a single `RunId` (`u32`, big-endian) identifying the run to stop.
pub fn parse_terminate_payload(payload: &[u8]) -> io::Result<u32> {
    let mut r = PayloadReader::new(payload);
    let run_id = r.read_u32()?;
    r.assert_done()?;
    Ok(run_id)
}

/// Convert a raw type byte into a known TS->Rust message type.
///
/// You can use this after `read_frame()` when reading from a host connection.
/// Unknown bytes should return `InvalidData`.
/// Parsed `StreamChunk` payload: `u32 runId, u32 streamId, Bytes data`.
pub struct StreamChunkPayload<'a> {
    pub run_id: u32,
    pub stream_id: u32,
    pub data: &'a [u8],
}

/// Parse a `StreamChunk` frame payload (borrowing the chunk bytes).
pub fn parse_stream_chunk_payload(payload: &[u8]) -> io::Result<StreamChunkPayload<'_>> {
    if payload.len() < 12 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "payload too short for StreamChunk",
        ));
    }
    let run_id = u32::from_be_bytes(payload[0..4].try_into().unwrap());
    let stream_id = u32::from_be_bytes(payload[4..8].try_into().unwrap());
    let len = u32::from_be_bytes(payload[8..12].try_into().unwrap());
    if len > STREAM_CHUNK_MAX_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("stream chunk of {len} bytes exceeds the {STREAM_CHUNK_MAX_BYTES} cap"),
        ));
    }
    if payload.len() != 12 + len as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "StreamChunk payload length mismatch",
        ));
    }
    Ok(StreamChunkPayload {
        run_id,
        stream_id,
        data: &payload[12..],
    })
}

/// Parsed `StreamEnd` payload: `u32 runId, u32 streamId, bool ok,
/// Optional<String> error` (present when `ok = false`).
pub struct StreamEndPayload {
    pub run_id: u32,
    pub stream_id: u32,
    pub ok: bool,
    pub error: Option<String>,
}

/// Parse a `StreamEnd` frame payload.
pub fn parse_stream_end_payload(payload: &[u8]) -> io::Result<StreamEndPayload> {
    let mut r = PayloadReader::new(payload);
    let run_id = r.read_u32()?;
    let stream_id = r.read_u32()?;
    let ok = r.read_bool()?;
    let error = if ok {
        None
    } else {
        match r.read_u8()? {
            0 => None,
            1 => Some(r.read_string()?),
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("invalid optional presence byte: {other:#04x}"),
                ))
            }
        }
    };
    r.assert_done()?;
    Ok(StreamEndPayload {
        run_id,
        stream_id,
        ok,
        error,
    })
}

/// Encode a `StreamPull` payload: `u32 runId, u32 streamId, u32 credit`.
pub fn encode_stream_pull_payload(run_id: u32, stream_id: u32, credit: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(12);
    out.extend_from_slice(&run_id.to_be_bytes());
    out.extend_from_slice(&stream_id.to_be_bytes());
    out.extend_from_slice(&credit.to_be_bytes());
    out
}

/// Encode a `StreamCancel` payload: `u32 runId, u32 streamId, String reason`.
pub fn encode_stream_cancel_payload(run_id: u32, stream_id: u32, reason: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(12 + reason.len());
    out.extend_from_slice(&run_id.to_be_bytes());
    out.extend_from_slice(&stream_id.to_be_bytes());
    out.extend_from_slice(&(reason.len() as u32).to_be_bytes());
    out.extend_from_slice(reason.as_bytes());
    out
}

pub fn parse_ts_to_rust_message_type(byte: u8) -> io::Result<TsToRustMessageType> {
    match byte {
        0x01 => Ok(TsToRustMessageType::Authenticate),
        0x02 => Ok(TsToRustMessageType::Run),
        0x03 => Ok(TsToRustMessageType::Precompile),
        0x04 => Ok(TsToRustMessageType::PrefixRun),
        0x05 => Ok(TsToRustMessageType::DisposePrefix),
        0x06 => Ok(TsToRustMessageType::BridgeResponse),
        0x07 => Ok(TsToRustMessageType::Terminate),
        0x08 => Ok(TsToRustMessageType::Stats),
        0x09 => Ok(TsToRustMessageType::StreamChunk),
        0x0A => Ok(TsToRustMessageType::StreamEnd),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unknown TS->Rust message type: {byte:#04x}"),
        )),
    }
}

/// Convert a raw type byte into a known Rust->TS message type.
///
/// This is mainly useful in tests and when validating outbound message tables.
pub fn parse_rust_to_ts_message_type(byte: u8) -> io::Result<RustToTsMessageType> {
    match byte {
        0x01 => Ok(RustToTsMessageType::BridgeCall),
        0x02 => Ok(RustToTsMessageType::Result),
        0x03 => Ok(RustToTsMessageType::PrecompileResult),
        0x04 => Ok(RustToTsMessageType::Log),
        0x05 => Ok(RustToTsMessageType::Hello),
        0x06 => Ok(RustToTsMessageType::StatsResult),
        0x07 => Ok(RustToTsMessageType::RunComplete),
        0x08 => Ok(RustToTsMessageType::StreamPull),
        0x09 => Ok(RustToTsMessageType::StreamCancel),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unknown Rust->TS message type: {byte:#04x}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A writer that accepts at most one byte per call — the adversarial
    /// case for the gather-write loop (every partial-write boundary is hit,
    /// including ones inside the 5-byte envelope and across slice edges).
    struct DribbleWriter(Vec<u8>);

    impl Write for DribbleWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if buf.is_empty() {
                return Ok(0);
            }
            self.0.push(buf[0]);
            Ok(1)
        }
        fn write_vectored(&mut self, bufs: &[io::IoSlice<'_>]) -> io::Result<usize> {
            for b in bufs {
                if !b.is_empty() {
                    self.0.push(b[0]);
                    return Ok(1);
                }
            }
            Ok(0)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn gather_write_reassembles_the_exact_frame_across_partial_writes() {
        let mut whole = Vec::new();
        write_rust_to_ts_frame_parts(
            &mut whole,
            RustToTsMessageType::BridgeCall,
            b"header",
            b"a larger tail payload",
        )
        .unwrap();

        let mut dribbled = DribbleWriter(Vec::new());
        write_rust_to_ts_frame_parts(
            &mut dribbled,
            RustToTsMessageType::BridgeCall,
            b"header",
            b"a larger tail payload",
        )
        .unwrap();

        assert_eq!(dribbled.0, whole, "partial writes must not reorder or drop bytes");

        // Empty pieces are legal (a payload-less frame, a header-only frame).
        let mut empty_tail = DribbleWriter(Vec::new());
        write_rust_to_ts_frame_parts(&mut empty_tail, RustToTsMessageType::Result, b"only", b"")
            .unwrap();
        let mut expected = Vec::new();
        write_rust_to_ts_frame_parts(&mut expected, RustToTsMessageType::Result, b"only", b"")
            .unwrap();
        assert_eq!(empty_tail.0, expected);
    }

    #[test]
    fn frame_roundtrip_preserves_type_and_payload() {
        let mut bytes = Vec::new();
        write_frame(&mut bytes, 0x02, b"hello").unwrap();

        let frame = read_frame(&mut bytes.as_slice()).unwrap();

        assert_eq!(
            frame,
            Frame {
                message_type: 0x02,
                payload: b"hello".to_vec(),
            }
        );
    }

    #[test]
    fn authenticate_payload_roundtrip_preserves_version_probe_and_token() {
        let auth = AuthenticatePayload {
            protocol_version: PROTOCOL_VERSION,
            probe: vec![0xff, 0x0f, 0x30],
            descriptor_token: vec![0xab; crate::webcodec::DESCRIPTOR_TOKEN_LEN],
        };

        let payload = encode_authenticate_payload(&auth);
        let parsed = parse_authenticate_payload(&payload).unwrap();

        assert_eq!(parsed, auth);
    }

    #[test]
    fn authenticate_payload_rejects_trailing_bytes() {
        let auth = AuthenticatePayload {
            protocol_version: PROTOCOL_VERSION,
            probe: vec![0xff, 0x0f, 0x30],
            descriptor_token: vec![0xab; crate::webcodec::DESCRIPTOR_TOKEN_LEN],
        };
        let mut v = encode_authenticate_payload(&auth);
        v.extend_from_slice(b"extra");

        let err = parse_authenticate_payload(&v).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert_eq!(err.to_string(), "Authenticate payload has trailing bytes");
    }

    #[test]
    fn authenticate_payload_rejects_truncated_probe() {
        // protocolVersion + a probe length that runs past the payload end.
        let mut v = Vec::new();
        v.extend_from_slice(&PROTOCOL_VERSION.to_be_bytes());
        push_u32(&mut v, 99);
        v.extend_from_slice(&[0xff, 0x0f]);

        let err = parse_authenticate_payload(&v).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert_eq!(err.to_string(), "Authenticate payload truncated (probe)");
    }

    #[test]
    fn stream_chunk_payload_roundtrips_and_caps() {
        let mut payload = Vec::new();
        payload.extend_from_slice(&7u32.to_be_bytes());
        payload.extend_from_slice(&3u32.to_be_bytes());
        payload.extend_from_slice(&4u32.to_be_bytes());
        payload.extend_from_slice(&[1, 2, 3, 4]);
        let chunk = parse_stream_chunk_payload(&payload).unwrap();
        assert_eq!(
            (chunk.run_id, chunk.stream_id, chunk.data),
            (7, 3, &[1u8, 2, 3, 4][..])
        );

        // Oversized chunk claim → malformed, before any allocation.
        let mut oversized = Vec::new();
        oversized.extend_from_slice(&7u32.to_be_bytes());
        oversized.extend_from_slice(&3u32.to_be_bytes());
        oversized.extend_from_slice(&(STREAM_CHUNK_MAX_BYTES + 1).to_be_bytes());
        assert!(parse_stream_chunk_payload(&oversized).is_err());

        // Length mismatch → malformed.
        let mut short = payload.clone();
        short.pop();
        assert!(parse_stream_chunk_payload(&short).is_err());
    }

    #[test]
    fn stream_end_payload_carries_the_error_only_on_failure() {
        let mut ok = Vec::new();
        ok.extend_from_slice(&7u32.to_be_bytes());
        ok.extend_from_slice(&3u32.to_be_bytes());
        ok.push(1); // ok
        let end = parse_stream_end_payload(&ok).unwrap();
        assert!(end.ok && end.error.is_none());

        let mut failed = Vec::new();
        failed.extend_from_slice(&7u32.to_be_bytes());
        failed.extend_from_slice(&3u32.to_be_bytes());
        failed.push(0); // ok = false
        failed.push(1); // error present
        push_string(&mut failed, "disk on fire");
        let end = parse_stream_end_payload(&failed).unwrap();
        assert_eq!((end.ok, end.error.as_deref()), (false, Some("disk on fire")));
    }

    #[test]
    fn stream_pull_and_cancel_encoders_match_the_documented_layout() {
        let pull = encode_stream_pull_payload(7, 3, 65536);
        assert_eq!(&pull[0..4], &7u32.to_be_bytes());
        assert_eq!(&pull[4..8], &3u32.to_be_bytes());
        assert_eq!(&pull[8..12], &65536u32.to_be_bytes());

        let cancel = encode_stream_cancel_payload(7, 3, "done");
        assert_eq!(&cancel[0..8], &pull[0..8]);
        assert_eq!(&cancel[8..12], &4u32.to_be_bytes());
        assert_eq!(&cancel[12..], b"done");
    }

    #[test]
    fn authenticate_payload_rejects_a_wrong_size_descriptor_token() {
        for wrong_len in [0usize, 8, 15, 17, 32] {
            let auth = AuthenticatePayload {
                protocol_version: PROTOCOL_VERSION,
                probe: vec![0xff, 0x0f, 0x30],
                descriptor_token: vec![0xab; wrong_len],
            };
            let err = parse_authenticate_payload(&encode_authenticate_payload(&auth)).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidData, "len {wrong_len}");
            assert!(
                err.to_string().contains("descriptor token must be exactly"),
                "len {wrong_len}: {err}"
            );
        }
    }

    #[test]
    fn authenticate_payload_rejects_a_missing_descriptor_token() {
        // The pre-token layout: version + probe, nothing after. The token is
        // required; both halves release in lockstep, so absence is malformed.
        let mut v = Vec::new();
        v.extend_from_slice(&PROTOCOL_VERSION.to_be_bytes());
        push_u32(&mut v, 3);
        v.extend_from_slice(&[0xff, 0x0f, 0x30]);

        let err = parse_authenticate_payload(&v).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert_eq!(err.to_string(), "Authenticate payload truncated (probe)");
    }

    #[test]
    fn hello_payload_encodes_status_probe_and_message() {
        let payload =
            encode_hello_payload(HelloStatus::V8FormatMismatch, &[0xff, 0x0f, 0x30], "no");
        assert_eq!(payload[0], HelloStatus::V8FormatMismatch as u8);
        assert_eq!(&payload[1..5], &3u32.to_be_bytes());
        assert_eq!(&payload[5..8], &[0xff, 0x0f, 0x30]);
        assert_eq!(&payload[8..12], &2u32.to_be_bytes());
        assert_eq!(&payload[12..], b"no");
    }

    #[test]
    fn hello_byte_matches_protocol_spec() {
        assert_eq!(RustToTsMessageType::Hello as u8, 0x05);
        assert_eq!(
            parse_rust_to_ts_message_type(0x05).unwrap(),
            RustToTsMessageType::Hello
        );
    }

    #[test]
    fn read_frame_rejects_zero_length_body() {
        let err = read_frame(&mut [0, 0, 0, 0].as_slice()).unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert_eq!(err.to_string(), "frame length cannot be zero");
    }

    #[test]
    fn parse_authenticate_payload_rejects_short_payload() {
        let err = parse_authenticate_payload(&[0x00]).unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert_eq!(err.to_string(), "payload too short for Authenticate");
    }

    #[test]
    fn read_frame_rejects_frame_larger_than_limit() {
        let err = read_frame_with_limit(&mut [0, 0, 0, 2, 0x01, 0x02].as_slice(), 1).unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert_eq!(err.to_string(), "frame length 2 exceeds max frame length 1");
    }

    #[test]
    fn write_frame_rejects_frame_larger_than_limit() {
        let mut bytes = Vec::new();
        let err = write_frame_with_limit(&mut bytes, 0x01, &[0x02], 1).unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(err.to_string(), "frame length 2 exceeds max frame length 1");
    }

    #[test]
    fn read_frame_rejects_truncated_body() {
        let err = read_frame(&mut [0, 0, 0, 3, 0x01, 0x02].as_slice()).unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn read_ts_to_rust_frame_casts_to_typed_frame() {
        let mut bytes = Vec::new();
        write_ts_to_rust_frame(&mut bytes, TsToRustMessageType::Run, b"payload").unwrap();

        let frame = read_ts_to_rust_frame(&mut bytes.as_slice()).unwrap();

        assert_eq!(
            frame,
            TypedFrame {
                message_type: TsToRustMessageType::Run,
                payload: b"payload".to_vec(),
            }
        );
    }

    #[test]
    fn read_rust_to_ts_frame_casts_to_typed_frame() {
        let mut bytes = Vec::new();
        write_rust_to_ts_frame(&mut bytes, RustToTsMessageType::Result, b"payload").unwrap();

        let frame = read_rust_to_ts_frame(&mut bytes.as_slice()).unwrap();

        assert_eq!(
            frame,
            TypedFrame {
                message_type: RustToTsMessageType::Result,
                payload: b"payload".to_vec(),
            }
        );
    }

    #[test]
    fn read_ts_to_rust_frame_rejects_unknown_message_type() {
        let mut bytes = Vec::new();
        write_frame(&mut bytes, 0xff, b"payload").unwrap();

        let err = read_ts_to_rust_frame(&mut bytes.as_slice()).unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert_eq!(err.to_string(), "unknown TS->Rust message type: 0xff");
    }

    #[test]
    fn read_rust_to_ts_frame_rejects_unknown_message_type() {
        let mut bytes = Vec::new();
        write_frame(&mut bytes, 0xff, b"payload").unwrap();

        let err = read_rust_to_ts_frame(&mut bytes.as_slice()).unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert_eq!(err.to_string(), "unknown Rust->TS message type: 0xff");
    }

    #[test]
    fn result_byte_matches_protocol_spec() {
        // docs/protocol.md §2.2: Result = 0x02
        assert_eq!(RustToTsMessageType::Result as u8, 0x02);
    }

    #[test]
    fn precompile_result_byte_matches_protocol_spec() {
        assert_eq!(RustToTsMessageType::PrecompileResult as u8, 0x03);
    }

    #[test]
    fn log_byte_matches_protocol_spec() {
        assert_eq!(RustToTsMessageType::Log as u8, 0x04);
    }

    #[test]
    fn stats_bytes_match_protocol_spec() {
        // docs/protocol.md §2.1: Stats = 0x08; §2.2: StatsResult = 0x06
        assert_eq!(TsToRustMessageType::Stats as u8, 0x08);
        assert_eq!(RustToTsMessageType::StatsResult as u8, 0x06);
        assert!(matches!(
            parse_ts_to_rust_message_type(0x08),
            Ok(TsToRustMessageType::Stats)
        ));
        assert!(matches!(
            parse_rust_to_ts_message_type(0x06),
            Ok(RustToTsMessageType::StatsResult)
        ));
    }

    // ── parse_run_payload ──────────────────────────────────────────────

    /// Build a minimal valid RunPayload byte vector.
    fn encode_run_payload(run_id: u32, code: &str, filename: Option<&str>) -> Vec<u8> {
        let mut v = Vec::new();
        push_u32(&mut v, run_id);
        push_string(&mut v, code);
        // filename: Optional<String>
        match filename {
            Some(f) => {
                v.push(1);
                push_string(&mut v, f);
            }
            None => {
                v.push(0);
            }
        }
        // ResourceLimits: 8 × Optional<u32>, all absent → runtime defaults.
        push_absent_limits(&mut v);
        push_u32(&mut v, 0); // globals count
        push_u32(&mut v, 0); // imports count
        v.push(0); // call: absent
        v
    }

    fn push_u32(v: &mut Vec<u8>, n: u32) {
        v.extend_from_slice(&n.to_be_bytes());
    }

    fn push_optional_u32(v: &mut Vec<u8>, n: Option<u32>) {
        match n {
            Some(x) => {
                v.push(1);
                push_u32(v, x);
            }
            None => v.push(0),
        }
    }

    /// Eight absent `Optional<u32>` limit fields (one presence byte each).
    fn push_absent_limits(v: &mut Vec<u8>) {
        v.extend_from_slice(&[0u8; 9]);
    }

    fn push_string(v: &mut Vec<u8>, s: &str) {
        let b = s.as_bytes();
        push_u32(v, b.len() as u32);
        v.extend_from_slice(b);
    }

    #[test]
    fn parse_run_payload_minimal() {
        let bytes = encode_run_payload(1, "export default 42", None);
        let p = parse_run_payload(&bytes).unwrap();
        assert_eq!(p.run_id, 1);
        assert_eq!(p.code, "export default 42");
        assert!(p.filename.is_none());
        assert!(p.globals.is_empty());
        assert!(p.imports.is_empty());
    }

    #[test]
    fn parse_run_payload_with_filename() {
        let bytes = encode_run_payload(7, "export default 1", Some("agent.js"));
        let p = parse_run_payload(&bytes).unwrap();
        assert_eq!(p.run_id, 7);
        assert_eq!(p.filename.as_deref(), Some("agent.js"));
    }

    #[test]
    fn parse_run_payload_with_limits() {
        let mut v = Vec::new();
        push_u32(&mut v, 42); // run_id
        push_string(&mut v, "code"); // code
        v.push(0); // no filename
        push_optional_u32(&mut v, Some(128)); // memory_mb
        push_optional_u32(&mut v, Some(5000)); // cpu_time_ms
        push_optional_u32(&mut v, Some(10000)); // wall_time_ms
        push_optional_u32(&mut v, Some(1024 * 1024)); // max_export_bytes
        push_optional_u32(&mut v, Some(512 * 1024)); // max_stdout_bytes
        push_optional_u32(&mut v, Some(512 * 1024)); // max_stderr_bytes
        push_optional_u32(&mut v, Some(64 * 1024)); // max_bridge_call_bytes
        push_optional_u32(&mut v, Some(1_000)); // max_bridge_calls
        push_optional_u32(&mut v, Some(5_000)); // grace_ms
        push_u32(&mut v, 0); // globals count
        push_u32(&mut v, 0); // imports count
        v.push(0); // call: absent

        let p = parse_run_payload(&v).unwrap();
        assert_eq!(p.run_id, 42);
        assert_eq!(p.limits.memory_mb, 128);
        assert_eq!(p.limits.cpu_time_ms, 5000);
        assert_eq!(p.limits.wall_time_ms, 10000);
        assert_eq!(p.limits.max_bridge_calls, 1_000);
    }

    #[test]
    fn parse_run_payload_absent_limits_resolve_to_defaults() {
        // All eight limit fields absent → the runtime fills each from its
        // DEFAULT_* constant. This is the whole point of the optional wire
        // encoding: the client no longer ships the default numbers.
        let bytes = encode_run_payload(1, "export default 1", None);
        let p = parse_run_payload(&bytes).unwrap();
        assert_eq!(p.limits.memory_mb, DEFAULT_MEMORY_MB);
        assert_eq!(p.limits.cpu_time_ms, DEFAULT_CPU_TIME_MS);
        assert_eq!(p.limits.wall_time_ms, DEFAULT_WALL_TIME_MS);
        assert_eq!(p.limits.max_export_bytes, DEFAULT_MAX_EXPORT_BYTES);
        assert_eq!(p.limits.max_stdout_bytes, DEFAULT_MAX_STDOUT_BYTES);
        assert_eq!(p.limits.max_stderr_bytes, DEFAULT_MAX_STDERR_BYTES);
        assert_eq!(
            p.limits.max_bridge_call_bytes,
            DEFAULT_MAX_BRIDGE_CALL_BYTES
        );
        assert_eq!(p.limits.max_bridge_calls, DEFAULT_MAX_BRIDGE_CALLS);
    }

    #[test]
    fn parse_run_payload_explicit_zero_limit_disables_not_defaults() {
        // An explicit 0 must survive as 0 (limit disabled), NOT be replaced by
        // the default — the client uses 0 to opt out of a cap.
        let mut v = Vec::new();
        push_u32(&mut v, 1); // run_id
        push_string(&mut v, "x"); // code
        v.push(0); // no filename
        push_optional_u32(&mut v, Some(0)); // memory_mb: explicitly unlimited
        push_optional_u32(&mut v, None); // cpu_time_ms: default
        push_optional_u32(&mut v, None); // wall_time_ms
        push_optional_u32(&mut v, None); // max_export_bytes
        push_optional_u32(&mut v, None); // max_stdout_bytes
        push_optional_u32(&mut v, None); // max_stderr_bytes
        push_optional_u32(&mut v, None); // max_bridge_call_bytes
        push_optional_u32(&mut v, Some(0)); // max_bridge_calls: explicitly unlimited
        push_optional_u32(&mut v, Some(0)); // grace_ms: epilogue explicitly off
        push_u32(&mut v, 0); // globals count
        push_u32(&mut v, 0); // imports count
        v.push(0); // call: absent

        let p = parse_run_payload(&v).unwrap();
        assert_eq!(p.limits.memory_mb, 0);
        assert_eq!(p.limits.max_bridge_calls, 0);
        // Untouched fields still resolve to their defaults.
        assert_eq!(p.limits.cpu_time_ms, DEFAULT_CPU_TIME_MS);
    }

    #[test]
    fn parse_run_payload_with_globals() {
        let mut v = Vec::new();
        push_u32(&mut v, 1); // run_id
        push_string(&mut v, "x"); // code
        v.push(0); // no filename
        push_absent_limits(&mut v); // limits (all absent → defaults)
        push_u32(&mut v, 2); // 2 globals
        v.push(0); // kind: bridge
        push_string(&mut v, "fetch");
        v.push(1); // enumerable
        v.push(0); // kind: bridge
        push_string(&mut v, "myTool");
        v.push(1); // enumerable
        push_u32(&mut v, 0); // 0 imports
        v.push(0); // call: absent

        let p = parse_run_payload(&v).unwrap();
        assert_eq!(p.globals.len(), 2);
        assert_eq!(p.globals[0].bridge_stub_name(), Some("fetch"));
        assert_eq!(p.globals[1].bridge_stub_name(), Some("myTool"));
    }

    #[test]
    fn parse_run_payload_with_all_global_kinds() {
        let mut v = Vec::new();
        push_u32(&mut v, 1); // run_id
        push_string(&mut v, "x"); // code
        v.push(0); // no filename
        push_absent_limits(&mut v); // limits
        push_u32(&mut v, 4); // 4 globals
                             // bridge (enumerable)
        v.push(0);
        push_string(&mut v, "fetch");
        v.push(1); // enumerable
        // string expr (non-enumerable — the flag parses on every kind)
        v.push(1);
        push_string(&mut v, "PI");
        v.push(0); // enumerable = false
        push_string(&mut v, "3.14159");
        // data: a value slot — u32 length + V8 serialization blob. The parser
        // carries the bytes verbatim (materialising them needs an isolate).
        v.push(2);
        push_string(&mut v, "flag");
        v.push(1); // enumerable
        push_u32(&mut v, 3);
        v.extend_from_slice(&[0xff, 0x0f, 0x54]);
        // shim
        v.push(3);
        push_string(&mut v, "wrapped");
        v.push(1); // enumerable
        push_string(&mut v, "(r) => r");
        push_string(&mut v, "__iso4_wrapped_h");
        push_u32(&mut v, 0); // 0 imports
        v.push(0); // call: absent

        let p = parse_run_payload(&v).unwrap();
        assert_eq!(p.globals.len(), 4);
        assert!(matches!(&p.globals[0], HostGlobalDef::Bridge { name, .. } if name == "fetch"));
        assert!(
            matches!(&p.globals[1], HostGlobalDef::StringExpr { name, expr, .. } if name == "PI" && expr == "3.14159")
        );
        assert!(p.globals[0].enumerable());
        assert!(!p.globals[1].enumerable());
        assert!(
            matches!(&p.globals[2], HostGlobalDef::Data { name, blob, .. } if name == "flag" && blob == &[0xff, 0x0f, 0x54])
        );
        assert!(matches!(
            &p.globals[3],
            HostGlobalDef::Shim { name, shim, handler_name, .. }
                if name == "wrapped" && shim == "(r) => r" && handler_name == "__iso4_wrapped_h"
        ));
        // Only bridge + shim install a stub.
        assert_eq!(p.globals[0].bridge_stub_name(), Some("fetch"));
        assert_eq!(p.globals[1].bridge_stub_name(), None);
        assert_eq!(p.globals[2].bridge_stub_name(), None);
        assert_eq!(p.globals[3].bridge_stub_name(), Some("__iso4_wrapped_h"));
    }

    #[test]
    fn parse_run_payload_with_one_source_import() {
        let mut v = Vec::new();
        push_u32(&mut v, 1); // run_id
        push_string(&mut v, "code"); // code
        v.push(0); // no filename
        push_absent_limits(&mut v); // limits
        push_u32(&mut v, 0); // globals count
        push_u32(&mut v, 1); // 1 import
        push_string(&mut v, "lib:math"); // specifier
        v.push(0); // kind: source
        push_string(&mut v, "export const add = (a, b) => a + b");
        v.push(0); // call: absent

        let p = parse_run_payload(&v).unwrap();
        assert_eq!(p.imports.len(), 1);
        assert_eq!(p.imports[0].specifier, "lib:math");
        assert!(matches!(
            &p.imports[0].module,
            ImportModule::Source(src) if src == "export const add = (a, b) => a + b"
        ));
    }

    #[test]
    fn parse_run_payload_with_multiple_source_imports() {
        let mut v = Vec::new();
        push_u32(&mut v, 1);
        push_string(&mut v, "code");
        v.push(0); // no filename
        push_absent_limits(&mut v); // limits
        push_u32(&mut v, 0); // globals count
        push_u32(&mut v, 2); // 2 imports
        push_string(&mut v, "lib:a");
        v.push(0); // kind: source
        push_string(&mut v, "export const a = 1");
        push_string(&mut v, "lib:b");
        v.push(0); // kind: source
        push_string(&mut v, "export const b = 2");
        v.push(0); // call: absent

        let p = parse_run_payload(&v).unwrap();
        assert_eq!(p.imports.len(), 2);
        assert_eq!(p.imports[0].specifier, "lib:a");
        assert!(matches!(
            &p.imports[0].module,
            ImportModule::Source(src) if src == "export const a = 1"
        ));
        assert_eq!(p.imports[1].specifier, "lib:b");
        assert!(matches!(
            &p.imports[1].module,
            ImportModule::Source(src) if src == "export const b = 2"
        ));
    }

    #[test]
    fn parse_run_payload_with_host_import_tree() {
        let mut v = Vec::new();
        push_u32(&mut v, 1); // run_id
        push_string(&mut v, "code");
        v.push(0); // no filename
        push_absent_limits(&mut v);
        push_u32(&mut v, 0); // globals count
        push_u32(&mut v, 1); // 1 import
        push_string(&mut v, "tools:search");
        v.push(1); // kind: host
        push_u32(&mut v, 3); // 3 top-level exports
                             // "query": function leaf
        push_string(&mut v, "query");
        v.push(0);
        // "config": data leaf — u32 length + V8 serialization blob.
        push_string(&mut v, "config");
        v.push(1);
        push_u32(&mut v, 3);
        v.extend_from_slice(&[0xff, 0x0f, 0x54]);
        // "nested": object with one function leaf "inner"
        push_string(&mut v, "nested");
        v.push(2);
        push_u32(&mut v, 1);
        push_string(&mut v, "inner");
        v.push(0);
        v.push(0); // call: absent

        let p = parse_run_payload(&v).unwrap();
        assert_eq!(p.imports.len(), 1);
        assert_eq!(p.imports[0].specifier, "tools:search");
        let ImportModule::Host(exports) = &p.imports[0].module else {
            panic!("expected host module");
        };
        assert_eq!(exports.len(), 3);
        assert_eq!(exports[0].0, "query");
        assert!(matches!(exports[0].1, HostModuleNode::Function));
        assert_eq!(exports[1].0, "config");
        assert!(matches!(&exports[1].1, HostModuleNode::Data(blob) if blob == &[0xff, 0x0f, 0x54]));
        assert_eq!(exports[2].0, "nested");
        let HostModuleNode::Object(entries) = &exports[2].1 else {
            panic!("expected object node");
        };
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, "inner");
        assert!(matches!(entries[0].1, HostModuleNode::Function));
    }

    #[test]
    fn parse_run_payload_rejects_unknown_import_kind() {
        let mut v = Vec::new();
        push_u32(&mut v, 1);
        push_string(&mut v, "code");
        v.push(0); // no filename
        push_absent_limits(&mut v);
        push_u32(&mut v, 0); // globals count
        push_u32(&mut v, 1); // 1 import
        push_string(&mut v, "lib:x");
        v.push(9); // bogus kind

        assert_eq!(
            parse_run_payload(&v).unwrap_err().kind(),
            io::ErrorKind::InvalidData,
        );
    }

    // ── List entry counts vs. the bytes actually present ───────────────
    //
    // A `List<…>` count is a size chosen by the sender, and the readers used
    // to hand it straight to `Vec::with_capacity`, which has no error path: a
    // request the allocator cannot serve aborts the process, and with
    // `panic = "abort"` in the release profile nothing contains that. These
    // cover the first of the two checks in `read_list` — a count is now
    // compared against the bytes left in the payload, so one that no payload
    // this size could back is rejected as malformed data before any memory is
    // asked for. Note the assertions are on
    // `InvalidData` specifically: an unchecked reader still fails these
    // payloads, but with `UnexpectedEof` from the middle of the entry loop,
    // which is the wrong answer for a length field that was wrong up front.

    #[test]
    fn parse_run_payload_rejects_global_def_count_past_payload_end() {
        let mut v = Vec::new();
        push_u32(&mut v, 1); // run_id
        push_string(&mut v, "x"); // code
        v.push(0); // no filename
        push_absent_limits(&mut v);
        push_u32(&mut v, 1000); // claims 1000 globals, supplies none

        let err = parse_run_payload(&v).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(
            err.to_string().contains("global def count 1000 exceeds"),
            "unexpected message: {err}"
        );
    }

    #[test]
    fn parse_run_payload_rejects_import_binding_count_past_payload_end() {
        let mut v = Vec::new();
        push_u32(&mut v, 1); // run_id
        push_string(&mut v, "x"); // code
        v.push(0); // no filename
        push_absent_limits(&mut v);
        push_u32(&mut v, 0); // globals count
        push_u32(&mut v, u32::MAX); // claims every import there could be

        let err = parse_run_payload(&v).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(
            err.to_string().contains("import binding count"),
            "unexpected message: {err}"
        );
    }

    #[test]
    fn parse_run_payload_rejects_host_module_export_count_past_payload_end() {
        let mut v = Vec::new();
        push_u32(&mut v, 1); // run_id
        push_string(&mut v, "code");
        v.push(0); // no filename
        push_absent_limits(&mut v);
        push_u32(&mut v, 0); // globals count
        push_u32(&mut v, 1); // 1 import
        push_string(&mut v, "tools:search");
        v.push(1); // kind: host
        push_u32(&mut v, u32::MAX); // claims every export there could be

        let err = parse_run_payload(&v).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(
            err.to_string().contains("host-module export count"),
            "unexpected message: {err}"
        );
    }

    #[test]
    fn parse_run_payload_rejects_host_module_object_entry_count_past_payload_end() {
        let mut v = Vec::new();
        push_u32(&mut v, 1); // run_id
        push_string(&mut v, "code");
        v.push(0); // no filename
        push_absent_limits(&mut v);
        push_u32(&mut v, 0); // globals count
        push_u32(&mut v, 1); // 1 import
        push_string(&mut v, "tools:search");
        v.push(1); // kind: host
        push_u32(&mut v, 1); // 1 top-level export
        push_string(&mut v, "nested");
        v.push(2); // node: object
        push_u32(&mut v, 1000); // claims 1000 entries, supplies none

        let err = parse_run_payload(&v).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(
            err.to_string()
                .contains("host-module object entry count 1000 exceeds"),
            "unexpected message: {err}"
        );
    }

    #[test]
    fn parse_prefix_run_payload_rejects_import_rebind_count_past_payload_end() {
        let mut v = Vec::new();
        push_u32(&mut v, 3); // run_id
        push_string(&mut v, "prefix-0"); // prefix_id
        v.push(1); // code: present
        push_string(&mut v, "code");
        v.push(0); // no filename
        push_absent_limits(&mut v);
        push_u32(&mut v, 0); // globals count
        push_u32(&mut v, u32::MAX); // claims every rebind there could be

        let err = parse_prefix_run_payload(&v).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(
            err.to_string().contains("import rebind count"),
            "unexpected message: {err}"
        );
    }

    #[test]
    fn parse_prefix_run_payload_reads_import_rebinds() {
        let mut v = Vec::new();
        push_u32(&mut v, 3); // run_id
        push_string(&mut v, "prefix-0"); // prefix_id
        v.push(1); // code: present
        push_string(&mut v, "code");
        v.push(0); // no filename
        push_absent_limits(&mut v);
        push_u32(&mut v, 0); // globals count
        push_u32(&mut v, 2); // 2 rebinds
        push_string(&mut v, "tools:search");
        push_string(&mut v, "query");
        push_string(&mut v, "tools:search");
        push_string(&mut v, "nested.inner");
        v.push(0); // call: absent

        let p = parse_prefix_run_payload(&v).unwrap();
        assert_eq!(p.code.as_deref(), Some("code"));
        assert_eq!(p.import_rebinds.len(), 2);
        assert_eq!(p.import_rebinds[0].specifier, "tools:search");
        assert_eq!(p.import_rebinds[0].path, "query");
        assert_eq!(p.import_rebinds[1].path, "nested.inner");
    }

    #[test]
    fn parse_run_payload_with_call() {
        let mut v = encode_run_payload(9, "export default { fetch() {} }", None);
        v.pop(); // replace the absent-call byte
        v.push(1); // call: present
        push_string(&mut v, "default.fetch"); // exportPath
        push_u32(&mut v, 3); // argsBlob: value slot
        v.extend_from_slice(&[0xff, 0x0f, 0x41]);

        let p = parse_run_payload(&v).unwrap();
        let call = p.call.expect("call spec");
        assert_eq!(call.export_path, "default.fetch");
        assert_eq!(call.args_blob, vec![0xff, 0x0f, 0x41]);
    }

    /// Build a PrefixRun payload with the given code / call presence.
    fn encode_prefix_run_payload(code: Option<&str>, call: Option<&str>) -> Vec<u8> {
        let mut v = Vec::new();
        push_u32(&mut v, 1); // run_id
        push_string(&mut v, "prefix-0"); // prefix_id
        match code {
            Some(c) => {
                v.push(1);
                push_string(&mut v, c);
            }
            None => v.push(0),
        }
        v.push(0); // no filename
        push_absent_limits(&mut v);
        push_u32(&mut v, 0); // globals count
        push_u32(&mut v, 0); // rebinds count
        match call {
            Some(path) => {
                v.push(1);
                push_string(&mut v, path);
                push_u32(&mut v, 3); // argsBlob: value slot
                v.extend_from_slice(&[0xff, 0x0f, 0x41]);
            }
            None => v.push(0),
        }
        v
    }

    #[test]
    fn parse_prefix_run_payload_with_call_only() {
        let p = parse_prefix_run_payload(&encode_prefix_run_payload(None, Some("named"))).unwrap();
        assert!(p.code.is_none());
        let call = p.call.expect("call spec");
        assert_eq!(call.export_path, "named");
        assert_eq!(call.args_blob, vec![0xff, 0x0f, 0x41]);
    }

    #[test]
    fn parse_prefix_run_payload_rejects_code_and_call_together() {
        assert_eq!(
            parse_prefix_run_payload(&encode_prefix_run_payload(Some("code"), Some("named")))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData,
        );
    }

    #[test]
    fn parse_prefix_run_payload_rejects_neither_code_nor_call() {
        assert_eq!(
            parse_prefix_run_payload(&encode_prefix_run_payload(None, None))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData,
        );
    }

    #[test]
    fn parse_terminate_payload_reads_run_id() {
        let payload = 42u32.to_be_bytes();
        assert_eq!(parse_terminate_payload(&payload).unwrap(), 42);
    }

    #[test]
    fn parse_terminate_payload_rejects_trailing_bytes() {
        let mut payload = 1u32.to_be_bytes().to_vec();
        payload.push(0xff);
        assert_eq!(
            parse_terminate_payload(&payload).unwrap_err().kind(),
            io::ErrorKind::InvalidData,
        );
    }

    #[test]
    fn parse_run_payload_rejects_truncated() {
        // Only run_id, missing everything else
        let v = 1u32.to_be_bytes().to_vec();
        assert_eq!(
            parse_run_payload(&v).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof,
        );
    }

    #[test]
    fn parse_run_payload_rejects_trailing_bytes() {
        let mut bytes = encode_run_payload(1, "code", None);
        bytes.push(0xde);
        bytes.push(0xad);
        assert_eq!(
            parse_run_payload(&bytes).unwrap_err().kind(),
            io::ErrorKind::InvalidData,
        );
    }
}
