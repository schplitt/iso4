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

use crate::wire::{decode_wire_value, WireValue};

/// Current wire protocol version.
///
/// This must stay in sync with `docs/protocol.md` and the future TypeScript
/// codec in `packages/iso4-dynamic/src/ipc.ts`.
pub const PROTOCOL_VERSION: u16 = 1;

/// Default maximum frame length in bytes, including the 1-byte message type.
pub const DEFAULT_MAX_FRAME_LENGTH: u32 = 64 * 1024 * 1024;

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
/// - first 2 bytes: protocol version (`u16`, big-endian)
/// - remaining bytes: UTF-8 token
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthenticatePayload {
    pub protocol_version: u16,
    pub token: String,
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

/// Parse the payload bytes of an `Authenticate` frame.
pub fn parse_authenticate_payload(payload: &[u8]) -> io::Result<AuthenticatePayload> {
    if payload.len() < 2 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "payload too short for Authenticate",
        ));
    }
    let protocol_version = u16::from_be_bytes([payload[0], payload[1]]);
    let token_bytes = &payload[2..];
    let token = String::from_utf8(token_bytes.to_vec())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "token is not valid UTF-8"))?;
    Ok(AuthenticatePayload {
        protocol_version,
        token,
    })
}

/// Encode an `Authenticate` payload from structured fields.
///
/// This is optional for the first Rust milestone, but useful for tests and for
/// understanding the inverse of `parse_authenticate_payload`.
pub fn encode_authenticate_payload(auth: &AuthenticatePayload) -> Vec<u8> {
    let mut payload = Vec::with_capacity(2 + auth.token.len());
    payload.extend_from_slice(&auth.protocol_version.to_be_bytes());
    payload.extend_from_slice(auth.token.as_bytes());
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
pub const DEFAULT_MEMORY_MB: u32 = 64;
pub const DEFAULT_CPU_TIME_MS: u32 = 5_000;
pub const DEFAULT_WALL_TIME_MS: u32 = 30_000;
pub const DEFAULT_MAX_EXPORT_BYTES: u32 = 16 * 1024 * 1024;
pub const DEFAULT_MAX_STDOUT_BYTES: u32 = 1024 * 1024;
pub const DEFAULT_MAX_STDERR_BYTES: u32 = 1024 * 1024;
pub const DEFAULT_MAX_BRIDGE_CALL_BYTES: u32 = 16 * 1024 * 1024;
pub const DEFAULT_MAX_BRIDGE_CALLS: u32 = 10;

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
        }
    }
}

/// A host global the sandbox is allowed to reference, tagged by how the
/// runtime installs it natively. Mirrors `GlobalDefPayload` on the TS side
/// (`ipc.ts`); see `docs/protocol.md` §5.2.
#[derive(Debug, Clone)]
pub enum HostGlobalDef {
    /// A plain host function — installed as a bridge stub under `name`.
    Bridge { name: String },
    /// A JS expression the runtime evaluates as its own script and sets on
    /// `globalThis[name]`.
    StringExpr { name: String, expr: String },
    /// A constant carried as a `WireValue`, materialised natively.
    Data { name: String, value: WireValue },
    /// A bridge handler (installed as a stub under `handler_name`) plus a shim
    /// expression the runtime wraps and sets on `globalThis[name]`.
    Shim {
        name: String,
        shim: String,
        handler_name: String,
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
            HostGlobalDef::Bridge { name } => Some(name),
            HostGlobalDef::Shim { handler_name, .. } => Some(handler_name),
            HostGlobalDef::StringExpr { .. } | HostGlobalDef::Data { .. } => None,
        }
    }

    /// A convenience constructor for tests: a plain bridge global.
    #[cfg(test)]
    pub fn bridge(name: &str) -> Self {
        HostGlobalDef::Bridge {
            name: name.to_string(),
        }
    }
}

/// One entry in the `imports` list of a `Run` / `Precompile` / `PrefixRun`
/// request. Always carries a source string: the TS-side import processor
/// lowers "host modules" (object form) to generated ESM source that calls
/// bridge globals at function leaves, so by the time a binding reaches the
/// wire it's source-form regardless of which public API flavor the host
/// used. See DESIGN.md §4.3.
#[derive(Debug, Clone)]
pub struct ImportBinding {
    pub specifier: String,
    pub source: String,
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
        })
    }

    /// Decode a single `WireValue` at the current cursor, advancing past it.
    fn read_wire_value(&mut self) -> io::Result<WireValue> {
        decode_wire_value(self.data, &mut self.offset)
    }

    /// Read the `List<GlobalDef>` block: a `u32` count followed by one tagged
    /// entry per global. Mirrors `writeGlobalDefs` in the TS codec (`ipc.ts`)
    /// and `docs/protocol.md` §5.2.
    fn read_global_defs(&mut self) -> io::Result<Vec<HostGlobalDef>> {
        let count = self.read_u32()? as usize;
        let mut defs = Vec::with_capacity(count);
        for _ in 0..count {
            let kind = self.read_u8()?;
            let name = self.read_string()?;
            let def = match kind {
                0 => HostGlobalDef::Bridge { name },
                1 => HostGlobalDef::StringExpr {
                    name,
                    expr: self.read_string()?,
                },
                2 => HostGlobalDef::Data {
                    name,
                    value: self.read_wire_value()?,
                },
                3 => HostGlobalDef::Shim {
                    name,
                    shim: self.read_string()?,
                    handler_name: self.read_string()?,
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

/// Parse the shared code + filename + limits + globals + imports fields that
/// appear in `RunPayload`, `PrecompilePayload`, and `PrefixRunPayload`.
fn parse_code_fields(
    r: &mut PayloadReader,
) -> io::Result<(
    String,
    Option<String>,
    ResourceLimits,
    Vec<HostGlobalDef>,
    Vec<ImportBinding>,
)> {
    let code = r.read_string()?;
    let filename = r.read_optional_string()?;
    let limits = r.read_resource_limits()?;
    let globals = r.read_global_defs()?;
    let imports_count = r.read_u32()? as usize;
    let mut imports = Vec::with_capacity(imports_count);
    for _ in 0..imports_count {
        let specifier = r.read_string()?;
        let source = r.read_string()?;
        imports.push(ImportBinding { specifier, source });
    }
    Ok((code, filename, limits, globals, imports))
}

/// Parse the payload bytes of a `Run` frame per `docs/protocol.md` §5.2.
pub fn parse_run_payload(payload: &[u8]) -> io::Result<RunPayload> {
    let mut r = PayloadReader::new(payload);
    let run_id = r.read_u32()?;
    let (code, filename, limits, globals, imports) = parse_code_fields(&mut r)?;
    r.assert_done()?;
    Ok(RunPayload {
        run_id,
        code,
        filename,
        limits,
        globals,
        imports,
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
    let (code, filename, limits, globals, imports) = parse_code_fields(&mut r)?;
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
#[derive(Debug)]
pub struct PrefixRunPayload {
    pub run_id: u32,
    pub prefix_id: String,
    pub code: String,
    pub filename: Option<String>,
    pub limits: ResourceLimits,
    pub globals: Vec<HostGlobalDef>,
    pub imports: Vec<ImportBinding>,
}

/// Parse the payload bytes of a `PrefixRun` frame per `docs/protocol.md` §5.2.
pub fn parse_prefix_run_payload(payload: &[u8]) -> io::Result<PrefixRunPayload> {
    let mut r = PayloadReader::new(payload);
    let run_id = r.read_u32()?;
    let prefix_id = r.read_string()?;
    let (code, filename, limits, globals, imports) = parse_code_fields(&mut r)?;
    r.assert_done()?;
    Ok(PrefixRunPayload {
        run_id,
        prefix_id,
        code,
        filename,
        limits,
        globals,
        imports,
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
pub fn parse_ts_to_rust_message_type(byte: u8) -> io::Result<TsToRustMessageType> {
    match byte {
        0x01 => Ok(TsToRustMessageType::Authenticate),
        0x02 => Ok(TsToRustMessageType::Run),
        0x03 => Ok(TsToRustMessageType::Precompile),
        0x04 => Ok(TsToRustMessageType::PrefixRun),
        0x05 => Ok(TsToRustMessageType::DisposePrefix),
        0x06 => Ok(TsToRustMessageType::BridgeResponse),
        0x07 => Ok(TsToRustMessageType::Terminate),
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
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unknown Rust->TS message type: {byte:#04x}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn authenticate_payload_roundtrip_preserves_version_and_token() {
        let auth = AuthenticatePayload {
            protocol_version: PROTOCOL_VERSION,
            token: "secret-token".to_string(),
        };

        let payload = encode_authenticate_payload(&auth);
        let parsed = parse_authenticate_payload(&payload).unwrap();

        assert_eq!(parsed, auth);
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
    fn parse_authenticate_payload_rejects_invalid_utf8_token() {
        let err = parse_authenticate_payload(&[0x00, 0x01, 0xff]).unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert_eq!(err.to_string(), "token is not valid UTF-8");
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
        v.extend_from_slice(&[0u8; 8]);
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
        push_u32(&mut v, 0); // globals count
        push_u32(&mut v, 0); // imports count

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
        push_u32(&mut v, 0); // globals count
        push_u32(&mut v, 0); // imports count

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
        v.push(0); // kind: bridge
        push_string(&mut v, "myTool");
        push_u32(&mut v, 0); // 0 imports

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
                             // bridge
        v.push(0);
        push_string(&mut v, "fetch");
        // string expr
        v.push(1);
        push_string(&mut v, "PI");
        push_string(&mut v, "3.14159");
        // data: a WireValue::Bool(true) → tag 0x03 (TAG_TRUE), no body
        v.push(2);
        push_string(&mut v, "flag");
        v.push(0x03);
        // shim
        v.push(3);
        push_string(&mut v, "wrapped");
        push_string(&mut v, "(r) => r");
        push_string(&mut v, "__iso4_wrapped_h");
        push_u32(&mut v, 0); // 0 imports

        let p = parse_run_payload(&v).unwrap();
        assert_eq!(p.globals.len(), 4);
        assert!(matches!(&p.globals[0], HostGlobalDef::Bridge { name } if name == "fetch"));
        assert!(
            matches!(&p.globals[1], HostGlobalDef::StringExpr { name, expr } if name == "PI" && expr == "3.14159")
        );
        assert!(
            matches!(&p.globals[2], HostGlobalDef::Data { name, value } if name == "flag" && *value == WireValue::Bool(true))
        );
        assert!(matches!(
            &p.globals[3],
            HostGlobalDef::Shim { name, shim, handler_name }
                if name == "wrapped" && shim == "(r) => r" && handler_name == "__iso4_wrapped_h"
        ));
        // Only bridge + shim install a stub.
        assert_eq!(p.globals[0].bridge_stub_name(), Some("fetch"));
        assert_eq!(p.globals[1].bridge_stub_name(), None);
        assert_eq!(p.globals[2].bridge_stub_name(), None);
        assert_eq!(p.globals[3].bridge_stub_name(), Some("__iso4_wrapped_h"));
    }

    #[test]
    fn parse_run_payload_with_one_import() {
        let mut v = Vec::new();
        push_u32(&mut v, 1); // run_id
        push_string(&mut v, "code"); // code
        v.push(0); // no filename
        push_absent_limits(&mut v); // limits
        push_u32(&mut v, 0); // globals count
        push_u32(&mut v, 1); // 1 import
        push_string(&mut v, "lib:math"); // specifier
        push_string(&mut v, "export const add = (a, b) => a + b");

        let p = parse_run_payload(&v).unwrap();
        assert_eq!(p.imports.len(), 1);
        assert_eq!(p.imports[0].specifier, "lib:math");
        assert_eq!(p.imports[0].source, "export const add = (a, b) => a + b");
    }

    #[test]
    fn parse_run_payload_with_multiple_imports() {
        let mut v = Vec::new();
        push_u32(&mut v, 1);
        push_string(&mut v, "code");
        v.push(0); // no filename
        push_absent_limits(&mut v); // limits
        push_u32(&mut v, 0); // globals count
        push_u32(&mut v, 2); // 2 imports
        push_string(&mut v, "lib:a");
        push_string(&mut v, "export const a = 1");
        push_string(&mut v, "lib:b");
        push_string(&mut v, "export const b = 2");

        let p = parse_run_payload(&v).unwrap();
        assert_eq!(p.imports.len(), 2);
        assert_eq!(p.imports[0].specifier, "lib:a");
        assert_eq!(p.imports[0].source, "export const a = 1");
        assert_eq!(p.imports[1].specifier, "lib:b");
        assert_eq!(p.imports[1].source, "export const b = 2");
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
