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

use std::collections::HashMap;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::ipc;
use crate::v8 as sandbox;
use crate::wire;

/// State shared across all connection threads in the same Rust process.
///
/// Using a single shared store means a prefix compiled on connection 0 is
/// visible to connections 1, 2, … N — which is required for the TypeScript
/// pool to route any run to any free slot.
pub struct SharedState {
    /// Snapshots keyed by their string PrefixId.
    pub prefix_store: Mutex<HashMap<String, Vec<u8>>>,
    /// Monotonically increasing counter for generating unique PrefixIds.
    pub next_prefix_id: AtomicU64,
    /// Auth token — must match the token sent in every Authenticate frame.
    pub token: String,
}

impl SharedState {
    pub fn new(token: String) -> Self {
        Self {
            prefix_store: Mutex::new(HashMap::new()),
            next_prefix_id: AtomicU64::new(0),
            token,
        }
    }
}

pub fn handle_client(mut stream: UnixStream, shared: Arc<SharedState>) {
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

    if auth.token != shared.token {
        eprintln!("[iso4-v8] bad token — closing");
        return;
    }

    eprintln!("[iso4-v8] client authenticated");

    // ── Step 3: message loop ──────────────────────────────────────────────

    // Prefix store and ID counter are shared across all connections so a
    // prefix compiled on any slot is visible to all other slots in the pool.

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
                            let prefix_id = shared
                                .next_prefix_id
                                .fetch_add(1, Ordering::Relaxed)
                                .to_string();
                            eprintln!(
                                "[iso4-v8] precompile succeeded — prefix_id={prefix_id} \
                                 ({} snapshot bytes)",
                                snapshot_bytes.len()
                            );
                            // unwrap_or_else(|p| p.into_inner()): if a thread panicked
                            // while holding this lock, Rust marks the Mutex "poisoned"
                            // and .unwrap() would cascade-panic here too. PoisonError
                            // wraps the MutexGuard live at panic time; .into_inner()
                            // discards the poison flag and returns the guard. The
                            // HashMap is intact after a panic on insert/get/remove,
                            // so recovering is always safe. Same pattern on all three
                            // lock sites below.
                            shared
                                .prefix_store
                                .lock()
                                .unwrap_or_else(|p| p.into_inner())
                                .insert(prefix_id.clone(), snapshot_bytes);
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

                let snapshot_clone = shared
                    .prefix_store
                    .lock()
                    .unwrap_or_else(|p| p.into_inner()) // see poison comment above
                    .get(&payload.prefix_id)
                    .cloned();

                let result_payload = match snapshot_clone {
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
                            &snapshot_bytes,
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
                        let existed = shared
                            .prefix_store
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .remove(&prefix_id)
                            .is_some();
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
