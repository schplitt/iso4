//! Per-connection session handling.
//!
//! `handle_client` is the entry point for one authenticated connection.
//! It owns the message loop for the lifetime of that connection.

use std::os::unix::net::UnixStream;

use crate::ipc;
use crate::v8 as sandbox;

const EXPECTED_TOKEN: &str = "dev-token";

pub fn handle_client(mut stream: UnixStream) {
    // ── Step 1 & 2: authenticate ──────────────────────────────────────────

    let auth_frame = match ipc::read_ts_to_rust_frame(&mut stream) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("[iso4-v8] failed to read first frame: {e}");
            return;
        }
    };

    if auth_frame.message_type != ipc::TsToRustMessageType::Authenticate {
        eprintln!(
            "[iso4-v8] expected Authenticate, got {:?} — closing",
            auth_frame.message_type
        );
        return;
    }

    let auth = match ipc::parse_authenticate_payload(&auth_frame.payload) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("[iso4-v8] bad Authenticate payload: {e}");
            return;
        }
    };

    if auth.protocol_version != ipc::PROTOCOL_VERSION {
        eprintln!(
            "[iso4-v8] protocol version mismatch: got {}, expected {} — closing",
            auth.protocol_version,
            ipc::PROTOCOL_VERSION
        );
        return;
    }

    if auth.token != EXPECTED_TOKEN {
        eprintln!("[iso4-v8] bad token — closing");
        return;
    }

    eprintln!("[iso4-v8] client authenticated");

    // ── Step 3: message loop ──────────────────────────────────────────────

    loop {
        let frame = match ipc::read_ts_to_rust_frame(&mut stream) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("[iso4-v8] connection closed or error: {e}");
                break;
            }
        };

        match frame.message_type {
            ipc::TsToRustMessageType::Authenticate => {
                eprintln!("[iso4-v8] unexpected second Authenticate — closing");
                break;
            }
            ipc::TsToRustMessageType::Run => {
                eprintln!(
                    "[iso4-v8] Run received ({} payload bytes)",
                    frame.payload.len()
                );

                match sandbox::execute(&frame.payload) {
                    Ok(output) => {
                        if !output.stdout.is_empty() {
                            let mut payload = Vec::with_capacity(1 + output.stdout.len());
                            payload.push(0); // stdout
                            payload.extend_from_slice(output.stdout.as_bytes());
                            if let Err(e) = ipc::write_rust_to_ts_frame(
                                &mut stream,
                                ipc::RustToTsMessageType::StdioChunk,
                                &payload,
                            ) {
                                eprintln!("[iso4-v8] failed to write stdout chunk: {e}");
                                break;
                            }
                        }

                        if !output.stderr.is_empty() {
                            let mut payload = Vec::with_capacity(1 + output.stderr.len());
                            payload.push(1); // stderr
                            payload.extend_from_slice(output.stderr.as_bytes());
                            if let Err(e) = ipc::write_rust_to_ts_frame(
                                &mut stream,
                                ipc::RustToTsMessageType::StdioChunk,
                                &payload,
                            ) {
                                eprintln!("[iso4-v8] failed to write stderr chunk: {e}");
                                break;
                            }
                        }

                        // Phase 2+: replace with V8 ValueSerializer of RunResult.
                        // For now: send the stringified default export as bytes.
                        let bytes = output
                            .default_export
                            .as_deref()
                            .unwrap_or("undefined")
                            .as_bytes()
                            .to_vec();
                        if let Err(e) = ipc::write_rust_to_ts_frame(
                            &mut stream,
                            ipc::RustToTsMessageType::Result,
                            &bytes,
                        ) {
                            eprintln!("[iso4-v8] failed to write Result: {e}");
                            break;
                        }
                    }
                    Err(e) => {
                        // Execution failed. Log it and send an empty Result
                        // for now. Later this becomes a serialized RunFailure.
                        eprintln!("[iso4-v8] execute error: {e:?}");
                        if let Err(e) = ipc::write_rust_to_ts_frame(
                            &mut stream,
                            ipc::RustToTsMessageType::Result,
                            &[],
                        ) {
                            eprintln!("[iso4-v8] failed to write Result: {e}");
                            break;
                        }
                    }
                }
            }
            ipc::TsToRustMessageType::Terminate => {
                eprintln!("[iso4-v8] Terminate received — closing");
                break;
            }
            ipc::TsToRustMessageType::BridgeResponse => {
                eprintln!("[iso4-v8] unexpected BridgeResponse outside of run — closing");
                break;
            }
        }
    }
}
