//! Per-connection session handling.
//!
//! `handle_client` is the entry point for one authenticated connection.
//! It owns the message loop for the lifetime of that connection.
//!
//! Protocol notes:
//! - Logs (stdout/stderr) are captured during execution and returned inside
//!   the `Result` frame's `RunCompletionPayload`. There are no `StdioChunk`
//!   frames in the real protocol.
//! - Each `Run` currently sends a hard-coded `run_id = 0` because the `Run`
//!   payload is still a plain UTF-8 code string (Phase 1). Once `RunPayload`
//!   parsing lands, the `run_id` will be read from the payload.

use std::os::unix::net::UnixStream;

use crate::ipc;
use crate::v8 as sandbox;
use crate::wire;

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

                // Phase 1: payload is raw UTF-8 ESM source. run_id is hardcoded
                // to 0 until RunPayload parsing is implemented.
                let run_id: u32 = 0;

                let result_payload = match sandbox::execute(&frame.payload) {
                    Ok(output) => {
                        eprintln!("[iso4-v8] run succeeded in {}ms", output.duration_ms);
                        wire::encode_run_completion_payload(
                            run_id,
                            wire::RunCompletion::Success(wire::RunSuccessPayload {
                                exports: output.exports,
                                stdout: output.stdout,
                                stderr: output.stderr,
                                duration_ms: output.duration_ms as f64,
                            }),
                        )
                    }
                    Err(failure) => {
                        eprintln!(
                            "[iso4-v8] run failed in {}ms: {:?}",
                            failure.duration_ms, failure.error
                        );
                        wire::encode_run_completion_payload(
                            run_id,
                            wire::RunCompletion::Failure(wire::RunFailurePayload {
                                error: wire::run_error_to_payload(&failure.error),
                                stdout: failure.stdout,
                                stderr: failure.stderr,
                                duration_ms: failure.duration_ms as f64,
                            }),
                        )
                    }
                };

                if let Err(e) = ipc::write_rust_to_ts_frame(
                    &mut stream,
                    ipc::RustToTsMessageType::Result,
                    &result_payload,
                ) {
                    eprintln!("[iso4-v8] failed to write Result frame: {e}");
                    break;
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
