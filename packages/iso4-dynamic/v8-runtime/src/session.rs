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
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::ipc;
use crate::v8 as sandbox;
use crate::wire;

/// Snapshot + declared global names for one precompiled prefix.
pub struct PrefixData {
    pub snapshot: Vec<u8>,
    /// Global names declared at precompile time. PrefixRun globals must be a
    /// subset of this set; extras are rejected with ERR_UNDECLARED_BINDING.
    pub declared_globals: Vec<String>,
}

/// State shared across all connection threads in the same Rust process.
///
/// Using a single shared store means a prefix compiled on connection 0 is
/// visible to connections 1, 2, … N — which is required for the TypeScript
/// pool to route any run to any free slot.
pub struct SharedState {
    /// Prefix data keyed by their string PrefixId.
    pub prefix_store: Mutex<HashMap<String, PrefixData>>,
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
                    "[iso4-v8] Run {} received ({} code bytes, {} globals)",
                    payload.run_id,
                    payload.code.len(),
                    payload.globals.len(),
                );

                let globals: Vec<String> = payload
                    .globals.iter().map(|g| g.name.clone()).collect();
                let stream_fd = if globals.is_empty() {
                    None
                } else {
                    Some(stream.as_raw_fd())
                };
                let result_payload = match sandbox::execute(
                    &payload.code,
                    payload.filename.as_deref(),
                    sandbox::Limits {
                        wall_time_ms: payload.limits.wall_time_ms,
                        cpu_time_ms:  payload.limits.cpu_time_ms,
                        memory_mb:    payload.limits.memory_mb,
                        max_bridge_payload_bytes: payload.limits.max_bridge_payload_bytes,
                        max_bridge_calls: payload.limits.max_bridge_calls,
                    },
                    &globals,
                    stream_fd,
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
                            let declared_globals: Vec<String> = payload
                                .globals.iter().map(|g| g.name.clone()).collect();
                            eprintln!(
                                "[iso4-v8] precompile succeeded — prefix_id={prefix_id} \
                                 ({} snapshot bytes, {} declared globals)",
                                snapshot_bytes.len(),
                                declared_globals.len(),
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
                                .insert(prefix_id.clone(), PrefixData {
                                    snapshot: snapshot_bytes,
                                    declared_globals,
                                });
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

                let prefix_data_clone = shared
                    .prefix_store
                    .lock()
                    .unwrap_or_else(|p| p.into_inner()) // see poison comment above
                    .get(&payload.prefix_id)
                    .map(|d| (d.snapshot.clone(), d.declared_globals.clone()));

                let result_payload = match prefix_data_clone {
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
                    Some((snapshot_bytes, declared_globals)) => {
                        // ── ERR_UNDECLARED_BINDING check ─────────────────────────────────
                        // Every global in payload.globals must have been declared at
                        // precompile time. Adding a new name at run time would silently
                        // mutate the snapshot's global object shape, breaking the
                        // invariant that the prefix captures the full bridge surface.
                        let declared_set: std::collections::HashSet<&str> =
                            declared_globals.iter().map(String::as_str).collect();
                        let undeclared: Vec<&str> = payload.globals.iter()
                            .map(|g| g.name.as_str())
                            .filter(|name| !declared_set.contains(name))
                            .collect();
                        if let Some(name) = undeclared.first() {
                            let msg = format!(
                                "global '{name}' was not declared at precompile time"
                            );
                            eprintln!("[iso4-v8] PrefixRun {} — ERR_UNDECLARED_BINDING: {msg}",
                                payload.run_id);
                            wire::encode_run_completion_payload(
                                payload.run_id,
                                wire::RunCompletion::Failure(wire::RunFailurePayload {
                                    error: wire::RunErrorPayload {
                                        code: "ERR_UNDECLARED_BINDING".to_string(),
                                        name: "Error".to_string(),
                                        message: msg,
                                        stack: None,
                                    },
                                    stdout: Vec::new(),
                                    stderr: Vec::new(),
                                    duration_ms: 0.0,
                                }),
                            )
                        } else {

                        let run_globals: Vec<String> = payload
                            .globals.iter().map(|g| g.name.clone()).collect();
                        let stream_fd = if run_globals.is_empty() {
                            None
                        } else {
                            Some(stream.as_raw_fd())
                        };
                        eprintln!(
                            "[iso4-v8] PrefixRun {} (prefix_id={}, {} code bytes, {} globals)",
                            payload.run_id, payload.prefix_id, payload.code.len(),
                            run_globals.len(),
                        );
                        match sandbox::execute_with_prefix(
                            &snapshot_bytes,
                            &payload.code,
                            payload.filename.as_deref(),
                            sandbox::Limits {
                                wall_time_ms: payload.limits.wall_time_ms,
                                cpu_time_ms:  payload.limits.cpu_time_ms,
                                memory_mb:    payload.limits.memory_mb,
                                max_bridge_payload_bytes: payload.limits.max_bridge_payload_bytes,
                                max_bridge_calls: payload.limits.max_bridge_calls,
                            },
                            &run_globals,
                            stream_fd,
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
                        } // end undeclared check else branch
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
                // A late BridgeResponse arrives when a bridge handler resolves
                // after the run's wall timeout already killed the run on the
                // Rust side. The connection is reused for future runs, so we
                // simply discard the late frame rather than closing.
                eprintln!("[iso4-v8] ignoring late BridgeResponse (run already completed)");
            }
            ipc::TsToRustMessageType::Terminate => {
                eprintln!("[iso4-v8] Terminate received — closing");
                break;
            }
        }
    }
}
