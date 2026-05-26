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

/// Current wire protocol version.
///
/// This must stay in sync with `docs/protocol.md` and the future TypeScript
/// codec in `packages/iso4-dynamic/src/ipc.ts`.
pub const PROTOCOL_VERSION: u16 = 1;

/// Default maximum frame length in bytes, including the 1-byte message type.
pub const DEFAULT_MAX_FRAME_LENGTH: u32 = 64 * 1024 * 1024;

/// Message types sent from the TypeScript host to Rust.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TsToRustMessageType {
    /// First frame on a new connection.
    Authenticate = 0x01,
    /// Start a sandboxed execution.
    Run = 0x02,
    /// Reply to a `BridgeCall` sent by Rust.
    BridgeResponse = 0x03,
    /// Force-stop a running isolate.
    Terminate = 0x04,
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

/// Convert a raw type byte into a known TS->Rust message type.
///
/// You can use this after `read_frame()` when reading from a host connection.
/// Unknown bytes should return `InvalidData`.
pub fn parse_ts_to_rust_message_type(byte: u8) -> io::Result<TsToRustMessageType> {
    match byte {
        0x01 => Ok(TsToRustMessageType::Authenticate),
        0x02 => Ok(TsToRustMessageType::Run),
        0x03 => Ok(TsToRustMessageType::BridgeResponse),
        0x04 => Ok(TsToRustMessageType::Terminate),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unknown TS->Rust message type: {byte:#02x}"),
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
}
