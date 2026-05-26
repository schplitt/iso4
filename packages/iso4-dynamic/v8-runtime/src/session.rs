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

    // Prefix store: maps PrefixId → V8 snapshot bytes.
    // Scoped to this connection so each client has independent prefix state.
    let mut prefix_store: std::collections::HashMap<String, Vec<u8>> = std::collections::HashMap::new();
    let mut next_prefix_id: u64 = 0;

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
                let payload = match ipc::parse_run_payload(&frame.payload) {
                    Ok(p) => p,
                    Err(e) => {
                        eprintln!("[iso4-v8] malformed Run payload: {e} — closing");
                        break;
                    }
                };

                eprintln!(
                    "[iso4-v8] Run {} received ({} code bytes)",
                    payload.run_id,
                    payload.code.len()
                );

                let result_payload = match sandbox::execute(
                    &payload.code,
                    payload.filename.as_deref(),
                ) {
                    Ok(output) => {
                        eprintln!("[iso4-v8] run succeeded in {}ms", output.duration_ms);
                        wire::encode_run_completion_payload(
                            payload.run_id,
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
                            payload.run_id,
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
            ipc::TsToRustMessageType::Precompile => {
                let payload = match ipc::parse_precompile_payload(&frame.payload) {
                    Ok(p) => p,
                    Err(e) => {
                        eprintln!("[iso4-v8] malformed Precompile payload: {e} — closing");
                        break;
                    }
                };

                let result_payload =
                    match sandbox::precompile(&payload.code, payload.filename.as_deref()) {
                        Ok(snapshot_bytes) => {
                            next_prefix_id += 1;
                            let prefix_id = next_prefix_id.to_string();
                            eprintln!(
                                "[iso4-v8] precompile succeeded — prefix_id={prefix_id}                                  ({} snapshot bytes)",
                                snapshot_bytes.len()
                            );
                            prefix_store.insert(prefix_id.clone(), snapshot_bytes);
                            wire::encode_precompile_result_payload(Some(&prefix_id), None)
                        }
                        Err(failure) => {
                            eprintln!(
                                "[iso4-v8] precompile failed in {}ms: {:?}",
                                failure.duration_ms, failure.error
                            );
                            wire::encode_precompile_result_payload(
                                None,
                                Some(&wire::run_error_to_payload(&failure.error)),
                            )
                        }
                    };

                if let Err(e) = ipc::write_rust_to_ts_frame(
                    &mut stream,
                    ipc::RustToTsMessageType::PrecompileResult,
                    &result_payload,
                ) {
                    eprintln!("[iso4-v8] failed to write PrecompileResult frame: {e}");
                    break;
                }
            }
            ipc::TsToRustMessageType::PrefixRun => {
                let payload = match ipc::parse_prefix_run_payload(&frame.payload) {
                    Ok(p) => p,
                    Err(e) => {
                        eprintln!("[iso4-v8] malformed PrefixRun payload: {e} — closing");
                        break;
                    }
                };

                let result_payload = match prefix_store.get(&payload.prefix_id) {
                    None => {
                        eprintln!(
                            "[iso4-v8] PrefixRun {} — prefix_id={} not found",
                            payload.run_id, payload.prefix_id
                        );
                        wire::encode_run_completion_payload(
                            payload.run_id,
                            wire::RunCompletion::Failure(wire::RunFailurePayload {
                                error: wire::RunErrorPayload {
                                    code: "ERR_PREFIX_DISPOSED".to_string(),
                                    name: "Error".to_string(),
                                    message: format!(
                                        "prefix '{}' has been disposed or never existed",
                                        payload.prefix_id
                                    ),
                                    stack: None,
                                },
                                stdout: Vec::new(),
                                stderr: Vec::new(),
                                duration_ms: 0.0,
                            }),
                        )
                    }
                    Some(snapshot_bytes) => {
                        eprintln!(
                            "[iso4-v8] PrefixRun {} (prefix_id={}, {} code bytes)",
                            payload.run_id, payload.prefix_id, payload.code.len()
                        );
                        match sandbox::execute_with_prefix(
                            snapshot_bytes,
                            &payload.code,
                            payload.filename.as_deref(),
                        ) {
                            Ok(output) => {
                                eprintln!(
                                    "[iso4-v8] PrefixRun {} succeeded in {}ms",
                                    payload.run_id, output.duration_ms
                                );
                                wire::encode_run_completion_payload(
                                    payload.run_id,
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
                                    "[iso4-v8] PrefixRun {} failed in {}ms: {:?}",
                                    payload.run_id, failure.duration_ms, failure.error
                                );
                                wire::encode_run_completion_payload(
                                    payload.run_id,
                                    wire::RunCompletion::Failure(wire::RunFailurePayload {
                                        error: wire::run_error_to_payload(&failure.error),
                                        stdout: failure.stdout,
                                        stderr: failure.stderr,
                                        duration_ms: failure.duration_ms as f64,
                                    }),
                                )
                            }
                        }
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
            ipc::TsToRustMessageType::DisposePrefix => {
                match ipc::parse_dispose_prefix_payload(&frame.payload) {
                    Ok(prefix_id) => {
                        let existed = prefix_store.remove(&prefix_id).is_some();
                        eprintln!(
                            "[iso4-v8] DisposePrefix prefix_id={prefix_id} (existed={existed})"
                        );
                        // No response frame — DisposePrefix is fire-and-forget.
                    }
                    Err(e) => {
                        eprintln!("[iso4-v8] malformed DisposePrefix payload: {e} — closing");
                        break;
                    }
                }
            }
            ipc::TsToRustMessageType::BridgeResponse => {
                eprintln!("[iso4-v8] unexpected BridgeResponse outside of run — closing");
                break;
            }
            ipc::TsToRustMessageType::Terminate => {
                eprintln!("[iso4-v8] Terminate received — closing");
                break;
            }
        }
    }
}
