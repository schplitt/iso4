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
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::ipc;
use crate::v8 as sandbox;
use crate::wire;

/// True when any global in `defs` installs a bridge stub — a plain function, a
/// shim handler, or the `__iso4_call` import dispatcher. Such a run needs the
/// session socket for bridge calls; string/data-only runs do not.
fn needs_bridge_stub(defs: &[ipc::HostGlobalDef]) -> bool {
    defs.iter().any(|g| g.bridge_stub_name().is_some())
}

/// Snapshot + declared global / import shape for one precompiled prefix.
pub struct PrefixData {
    pub snapshot: Vec<u8>,
    /// Global names declared at precompile time. PrefixRun globals must be a
    /// subset of this set; extras are rejected with ERR_UNDECLARED_BINDING.
    pub declared_globals: Vec<String>,
    /// Imports declared at precompile time. Re-used as-is on PrefixRun: source
    /// modules and host data exports are frozen, and host import function
    /// shape (specifier + name) must match what the snapshot was compiled
    /// against. Run-time `imports` in the PrefixRun payload only carry
    /// re-binding of host function handlers (TS dispatch); the wire-level
    /// binding shape comes from here.
    pub declared_imports: Vec<ipc::ImportBinding>,
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

    // Per-connection bridge call-ID counter.  Monotonically increasing across
    // all runs on this connection so callIds never reset to 0 between runs.
    // This ensures that a stale BridgeResponse from a previous run's orphaned
    // TS handler is rejected by bridge_global_callback (its callId will be
    // less than the current run's first callId). See deferred-fixes D1.
    let call_id_counter = Arc::new(AtomicU32::new(0));

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

                // A socket is needed only when some global installs a bridge
                // stub (a plain function, a shim handler, or the `__iso4_call`
                // import dispatcher). String/data-only runs need no socket.
                let stream_fd = if needs_bridge_stub(&payload.globals) {
                    Some(stream.as_raw_fd())
                } else {
                    None
                };
                let result_payload = match sandbox::execute(
                    &payload.code,
                    payload.filename.as_deref(),
                    sandbox::Limits {
                        wall_time_ms: payload.limits.wall_time_ms,
                        cpu_time_ms: payload.limits.cpu_time_ms,
                        memory_mb: payload.limits.memory_mb,
                        max_export_bytes: payload.limits.max_export_bytes,
                        max_stdout_bytes: payload.limits.max_stdout_bytes,
                        max_stderr_bytes: payload.limits.max_stderr_bytes,
                        max_bridge_call_bytes: payload.limits.max_bridge_call_bytes,
                        max_bridge_calls: payload.limits.max_bridge_calls,
                    },
                    &payload.globals,
                    &payload.imports,
                    stream_fd,
                    Arc::clone(&call_id_counter),
                ) {
                    Ok(output) => {
                        eprintln!("[iso4-v8] run succeeded in {:.3}ms", output.duration_ms);
                        wire::encode_run_completion_payload(
                            payload.run_id,
                            wire::RunCompletion::Success(wire::RunSuccessPayload {
                                exports: output.exports,
                                stdout: output.stdout,
                                stderr: output.stderr,
                                duration_ms: output.duration_ms,
                                cpu_time_ms: output.cpu_time_ms,
                                bridge_calls: output.bridge_calls,
                            }),
                        )
                    }
                    Err(failure) => {
                        eprintln!(
                            "[iso4-v8] run failed in {:.3}ms: {:?}",
                            failure.duration_ms, failure.error
                        );
                        wire::encode_run_completion_payload(
                            payload.run_id,
                            wire::RunCompletion::Failure(wire::RunFailurePayload {
                                error: wire::run_error_to_payload(&failure.error),
                                stdout: failure.stdout,
                                stderr: failure.stderr,
                                duration_ms: failure.duration_ms,
                                cpu_time_ms: failure.cpu_time_ms,
                                bridge_calls: failure.bridge_calls,
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

                let result_payload = match sandbox::precompile(
                    &payload.code,
                    payload.filename.as_deref(),
                    &payload.globals,
                    &payload.imports,
                ) {
                    Ok(snapshot_bytes) => {
                        let prefix_id = shared
                            .next_prefix_id
                            .fetch_add(1, Ordering::Relaxed)
                            .to_string();
                        // Only bridge-backed globals are re-installable per run
                        // (string/data globals and shim wrappers are frozen in
                        // the snapshot). The declared set is the bridge stub
                        // names a PrefixRun may re-bind; the undeclared-binding
                        // check validates against it.
                        let declared_globals: Vec<String> = payload
                            .globals
                            .iter()
                            .filter_map(|g| g.bridge_stub_name().map(str::to_string))
                            .collect();
                        let declared_imports = payload.imports.clone();
                        eprintln!(
                            "[iso4-v8] precompile succeeded — prefix_id={prefix_id} \
                                 ({} snapshot bytes, {} declared globals, {} declared imports)",
                            snapshot_bytes.len(),
                            declared_globals.len(),
                            declared_imports.len(),
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
                            .insert(
                                prefix_id.clone(),
                                PrefixData {
                                    snapshot: snapshot_bytes,
                                    declared_globals,
                                    declared_imports,
                                },
                            );
                        wire::encode_precompile_result_payload(Some(&prefix_id), None)
                    }
                    Err(failure) => {
                        eprintln!(
                            "[iso4-v8] precompile failed in {:.3}ms: {:?}",
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
                    .map(|d| {
                        (
                            d.snapshot.clone(),
                            d.declared_globals.clone(),
                            d.declared_imports.clone(),
                        )
                    });

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
                                    fields: None,
                                },
                                stdout: Vec::new(),
                                stderr: Vec::new(),
                                duration_ms: 0.0,
                                cpu_time_ms: 0.0,
                                bridge_calls: Vec::new(),
                            }),
                        )
                    }
                    Some((snapshot_bytes, declared_globals, declared_imports)) => {
                        // ── ERR_UNDECLARED_BINDING check ─────────────────────────────────
                        // Every global in payload.globals must have been declared at
                        // precompile time. Adding a new name at run time would silently
                        // mutate the snapshot's global object shape, breaking the
                        // invariant that the prefix captures the full bridge surface.
                        let declared_set: std::collections::HashSet<&str> =
                            declared_globals.iter().map(String::as_str).collect();
                        let undeclared: Vec<&str> = payload
                            .globals
                            .iter()
                            .filter_map(|g| g.bridge_stub_name())
                            .filter(|name| !declared_set.contains(name))
                            .collect();
                        if let Some(name) = undeclared.first() {
                            let msg =
                                format!("global '{name}' was not declared at precompile time");
                            eprintln!(
                                "[iso4-v8] PrefixRun {} — ERR_UNDECLARED_BINDING: {msg}",
                                payload.run_id
                            );
                            wire::encode_run_completion_payload(
                                payload.run_id,
                                wire::RunCompletion::Failure(wire::RunFailurePayload {
                                    error: wire::RunErrorPayload {
                                        code: "ERR_UNDECLARED_BINDING".to_string(),
                                        name: "Error".to_string(),
                                        message: msg,
                                        stack: None,
                                        fields: None,
                                    },
                                    stdout: Vec::new(),
                                    stderr: Vec::new(),
                                    duration_ms: 0.0,
                                    cpu_time_ms: 0.0,
                                    bridge_calls: Vec::new(),
                                }),
                            )
                        } else {
                            // PrefixRun reuses the declared imports shape
                            // verbatim. The TS side enforces that run-time
                            // overrides only update host function handlers
                            // (data exports + source modules are frozen in
                            // the snapshot), so the wire-level shape that
                            // matters for bridge dispatch here is the one
                            // captured at precompile. Every run global here is
                            // a bridge stub to re-install (string/data globals
                            // and shim wrappers live in the snapshot).
                            let stream_fd = if needs_bridge_stub(&payload.globals) {
                                Some(stream.as_raw_fd())
                            } else {
                                None
                            };
                            eprintln!(
                            "[iso4-v8] PrefixRun {} (prefix_id={}, {} code bytes, {} globals, {} imports)",
                            payload.run_id, payload.prefix_id, payload.code.len(),
                            payload.globals.len(), declared_imports.len(),
                        );
                            match sandbox::execute_with_prefix(
                                &snapshot_bytes,
                                &payload.code,
                                payload.filename.as_deref(),
                                sandbox::Limits {
                                    wall_time_ms: payload.limits.wall_time_ms,
                                    cpu_time_ms: payload.limits.cpu_time_ms,
                                    memory_mb: payload.limits.memory_mb,
                                    max_export_bytes: payload.limits.max_export_bytes,
                                    max_stdout_bytes: payload.limits.max_stdout_bytes,
                                    max_stderr_bytes: payload.limits.max_stderr_bytes,
                                    max_bridge_call_bytes: payload.limits.max_bridge_call_bytes,
                                    max_bridge_calls: payload.limits.max_bridge_calls,
                                },
                                &payload.globals,
                                &declared_imports,
                                stream_fd,
                                Arc::clone(&call_id_counter),
                            ) {
                                Ok(output) => {
                                    eprintln!(
                                        "[iso4-v8] PrefixRun {} succeeded in {:.3}ms",
                                        payload.run_id, output.duration_ms
                                    );
                                    wire::encode_run_completion_payload(
                                        payload.run_id,
                                        wire::RunCompletion::Success(wire::RunSuccessPayload {
                                            exports: output.exports,
                                            stdout: output.stdout,
                                            stderr: output.stderr,
                                            duration_ms: output.duration_ms,
                                            cpu_time_ms: output.cpu_time_ms,
                                            bridge_calls: output.bridge_calls,
                                        }),
                                    )
                                }
                                Err(failure) => {
                                    eprintln!(
                                        "[iso4-v8] PrefixRun {} failed in {:.3}ms: {:?}",
                                        payload.run_id, failure.duration_ms, failure.error
                                    );
                                    wire::encode_run_completion_payload(
                                        payload.run_id,
                                        wire::RunCompletion::Failure(wire::RunFailurePayload {
                                            error: wire::run_error_to_payload(&failure.error),
                                            stdout: failure.stdout,
                                            stderr: failure.stderr,
                                            duration_ms: failure.duration_ms,
                                            cpu_time_ms: failure.cpu_time_ms,
                                            bridge_calls: failure.bridge_calls,
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
                // A Terminate reaching the top-level session loop targets a run
                // that has already completed — its Result was sent just before
                // the host's abort landed (a benign race). The in-flight case is
                // consumed inside the run's poll loop (see v8.rs), which returns
                // an ERR_ABORTED Result. Discard the stray frame and keep the
                // connection healthy for reuse, mirroring the late-BridgeResponse
                // arm above; closing here would needlessly force the pool to
                // reconnect the slot.
                match ipc::parse_terminate_payload(&frame.payload) {
                    Ok(run_id) => eprintln!(
                        "[iso4-v8] ignoring stray Terminate (run {run_id} already completed)"
                    ),
                    Err(e) => {
                        eprintln!("[iso4-v8] ignoring stray Terminate with malformed payload: {e}")
                    }
                }
            }
        }
    }
}
