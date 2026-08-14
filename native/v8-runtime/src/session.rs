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

/// Per-run trace lines (`Run`/`PrefixRun` received / succeeded / failed) are
/// **off by default**. They are two unbuffered `eprintln!`s on the hot path,
/// and the runtime's stderr is inherited by the host process, so they cost
/// ~13 µs per run when stderr is discarded and ~23 µs when it is a real file or
/// pipe — 2-4 % of a hot `prefix.execute()` (measured 2026-08-10, release
/// build, 10-core Apple Silicon).
///
/// Set `ISO4_V8_TRACE=1` to get them back. Everything else the runtime logs —
/// handshake failures, protocol violations, prefix lifecycle, frame-write
/// errors — is unconditional: those are rare, and they are the only signal for
/// failures the host cannot observe from a `Result` frame.
fn trace_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        matches!(
            std::env::var("ISO4_V8_TRACE").as_deref(),
            Ok("1") | Ok("true")
        )
    })
}

/// `eprintln!` gated on [`trace_enabled`]. Arguments are only evaluated when
/// tracing is on, so a disabled trace line costs one relaxed atomic load.
macro_rules! trace {
    ($($arg:tt)*) => {
        if crate::session::trace_enabled() {
            eprintln!($($arg)*);
        }
    };
}

/// True when the run needs the session socket for bridge calls: some global
/// installs a bridge stub (a plain function or a shim handler), or some host
/// import declares a function leaf (the runtime then installs the reserved
/// `__iso4_call` dispatcher stub itself). String/data-only runs do not.
fn needs_bridge_stub(defs: &[ipc::HostGlobalDef], imports: &[ipc::ImportBinding]) -> bool {
    defs.iter().any(|g| g.bridge_stub_name().is_some())
        || imports.iter().any(|b| match &b.module {
            ipc::ImportModule::Source(_) => false,
            ipc::ImportModule::Host(exports) => has_function_leaf(exports),
        })
}

fn has_function_leaf(entries: &[(String, ipc::HostModuleNode)]) -> bool {
    entries.iter().any(|(_, node)| match node {
        ipc::HostModuleNode::Function => true,
        ipc::HostModuleNode::Object(children) => has_function_leaf(children),
        ipc::HostModuleNode::Data(_) => false,
    })
}

/// Validate the host-import rebinds a `PrefixRun` requested against the shape
/// declared at `Precompile`. Returns the first violation, phrased like the
/// undeclared-globals check: only declared host-module *function leaves* may
/// be re-pointed at a new host handler; source modules and data leaves are
/// frozen with the snapshot.
fn validate_import_rebinds(
    rebinds: &[ipc::ImportRebind],
    declared: &[ipc::ImportBinding],
) -> Result<(), String> {
    for rb in rebinds {
        let Some(binding) = declared.iter().find(|b| b.specifier == rb.specifier) else {
            return Err(format!(
                "import '{}' was not declared at precompile time",
                rb.specifier
            ));
        };
        let exports = match &binding.module {
            ipc::ImportModule::Source(_) => {
                return Err(format!(
                    "import '{}' is a source module — source imports are frozen \
                     in the snapshot and cannot be rebound at prefix.run() time",
                    rb.specifier
                ))
            }
            ipc::ImportModule::Host(exports) => exports,
        };
        match find_host_node(exports, &rb.path) {
            Some(ipc::HostModuleNode::Function) => {}
            Some(ipc::HostModuleNode::Data(_)) => {
                return Err(format!(
                    "import '{}'.{} is a data leaf, not a function — data leaves \
                     cannot be rebound",
                    rb.specifier, rb.path
                ))
            }
            _ => {
                return Err(format!(
                    "import '{}'.{} was not declared at precompile time",
                    rb.specifier, rb.path
                ))
            }
        }
    }
    Ok(())
}

/// Follow a dot-joined path through a host-module tree.
fn find_host_node<'a>(
    entries: &'a [(String, ipc::HostModuleNode)],
    path: &str,
) -> Option<&'a ipc::HostModuleNode> {
    let mut segments = path.split('.');
    let first = segments.next()?;
    let mut node = entries.iter().find(|(k, _)| k == first).map(|(_, n)| n)?;
    for segment in segments {
        let ipc::HostModuleNode::Object(children) = node else {
            return None;
        };
        node = children
            .iter()
            .find(|(k, _)| k == segment)
            .map(|(_, n)| n)?;
    }
    Some(node)
}

/// Validated source + declared global / import shape for one prepared prefix.
///
/// Stored behind an `Arc` in the store so a `PrefixRun` takes a handle under
/// the lock instead of deep-cloning the source and global defs on every run.
/// Each run re-evaluates the source into its fresh context; there is no
/// runtime snapshot (V8 14.x cannot create snapshots safely in a live
/// multi-isolate process — #60/#61).
pub struct PrefixData {
    /// The prefix module source, exactly as validated at precompile time.
    pub code: String,
    /// Filename the prefix was declared with (module origin in stacks).
    pub filename: Option<String>,
    /// The full precompile-time global defs, replayed into every run's
    /// context before the prefix evaluates (value globals as values, bridge
    /// callables as throwing placeholders).
    pub globals: Vec<ipc::HostGlobalDef>,
    /// Global names declared at precompile time. PrefixRun globals must be a
    /// subset of this set; extras are rejected with ERR_UNDECLARED_BINDING.
    pub declared_globals: Vec<String>,
    /// Imports declared at precompile time. Re-used as-is on PrefixRun:
    /// source modules and host data exports are frozen at declaration, and
    /// host import function shape (specifier + name) must match what the
    /// prefix was validated against. Run-time `imports` in the PrefixRun
    /// payload only carry re-binding of host function handlers (TS
    /// dispatch); the wire-level binding shape comes from here.
    pub declared_imports: Vec<ipc::ImportBinding>,
}

/// State shared across all connection threads in the same Rust process.
///
/// Using a single shared store means a prefix compiled on connection 0 is
/// visible to connections 1, 2, … N — which is required for the TypeScript
/// pool to route any run to any free slot.
pub struct SharedState {
    /// Prefix data keyed by their string PrefixId.
    pub prefix_store: Mutex<HashMap<String, Arc<PrefixData>>>,
    /// Monotonically increasing counter for generating unique PrefixIds.
    pub next_prefix_id: AtomicU64,
    /// Auth token — must match the token sent in every Authenticate frame.
    pub token: String,
    /// Warm instance registry (#64): every PrefixRun is served by a resident
    /// isolate from here; one-off runs share the same slot accounting.
    pub warm: crate::warm::WarmRegistry,
}

impl SharedState {
    /// `warm_budget_bytes` is the RSS mark the registry sheds against
    /// (#66) — the memory control; 0 disables it. Concurrency is bounded
    /// by the host pool; there is no instance-count cap.
    pub fn new(token: String, warm_budget_bytes: u64) -> Self {
        Self {
            prefix_store: Mutex::new(HashMap::new()),
            next_prefix_id: AtomicU64::new(0),
            token,
            warm: crate::warm::WarmRegistry::new(warm_budget_bytes),
        }
    }
}

/// Best-effort `Hello` write on a rejected handshake. The connection is about
/// to close either way, so a write failure here is only worth a log line.
fn send_hello(stream: &mut UnixStream, status: ipc::HelloStatus, message: &str) {
    if let Err(e) = ipc::write_rust_to_ts_frame(
        stream,
        ipc::RustToTsMessageType::Hello,
        &ipc::encode_hello_payload(status, crate::blob::probe(), message),
    ) {
        eprintln!("[iso4-v8] failed to write rejection Hello frame: {e}");
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
            // The payload shape itself is wrong — the peer is not speaking this
            // protocol at all, so there is nothing meaningful to reply with.
            eprintln!("[iso4-v8] bad Authenticate payload: {e}");
            return;
        }
    };

    if auth.protocol_version != ipc::PROTOCOL_VERSION {
        let message = format!(
            "iso4 protocol version mismatch: host speaks v{}, this iso4-v8 binary speaks v{}. \
             Update @iso4/sandbox and @iso4/v8-* together — they are released in lockstep.",
            auth.protocol_version,
            ipc::PROTOCOL_VERSION
        );
        eprintln!("[iso4-v8] {message} — closing");
        send_hello(
            &mut stream,
            ipc::HelloStatus::ProtocolVersionMismatch,
            &message,
        );
        return;
    }

    // ── V8 serialization format check ────────────────────────────────────────
    // Values cross this socket as V8 serialization blobs, so both V8s must
    // agree on the format. The host's probe is a serialized `null` whose second
    // byte is the format version Node writes; V8's ReadHeader hard-rejects
    // anything newer than the reader knows. This is a byte comparison — the
    // probe for this binary was computed once at process start in a throwaway
    // isolate, so no isolate plumbing reaches the session layer.
    let host_version = crate::blob::probe_format_version(&auth.probe);
    let own_version = crate::blob::write_format_version();
    match host_version {
        Some(v) if v <= own_version => {}
        _ => {
            let message = format!(
                "V8 serialization format mismatch between Node (writes format {}) and the \
                 iso4-v8 binary (reads up to format {own_version}). \
                 Update @iso4/sandbox and @iso4/v8-* together — they are released in lockstep.",
                host_version
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "an unrecognised format".to_string()),
            );
            eprintln!("[iso4-v8] {message} — closing");
            send_hello(&mut stream, ipc::HelloStatus::V8FormatMismatch, &message);
            return;
        }
    }

    if auth.token != shared.token {
        // Deliberately silent: an unauthenticated peer learns nothing about
        // why it was refused.
        eprintln!("[iso4-v8] bad token — closing");
        return;
    }

    if let Err(e) = ipc::write_rust_to_ts_frame(
        &mut stream,
        ipc::RustToTsMessageType::Hello,
        &ipc::encode_hello_payload(ipc::HelloStatus::Ok, crate::blob::probe(), ""),
    ) {
        eprintln!("[iso4-v8] failed to write Hello frame: {e}");
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

                trace!(
                    "[iso4-v8] Run {} received ({} code bytes, {} globals, call={})",
                    payload.run_id,
                    payload.code.len(),
                    payload.globals.len(),
                    payload
                        .call
                        .as_ref()
                        .map(|c| c.export_path.as_str())
                        .unwrap_or("-"),
                );

                // A socket is needed only when some global installs a bridge
                // stub or some host import declares a function leaf.
                // String/data-only runs need no socket.
                let stream_fd = if needs_bridge_stub(&payload.globals, &payload.imports) {
                    Some(stream.as_raw_fd())
                } else {
                    None
                };
                // One-off runs always get a fresh isolate (never the warm
                // registry), but they share the slot budget with warm
                // instances — taking a slot may evict an idle one (#64).
                shared.warm.reserve_oneoff();
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
                    payload.call.as_ref(),
                ) {
                    Ok(output) => {
                        trace!("[iso4-v8] run succeeded in {:.3}ms", output.duration_ms);
                        wire::encode_run_completion_payload(
                            payload.run_id,
                            wire::RunCompletion::Success(wire::RunSuccessPayload {
                                exports: output.exports,
                                skipped_exports: output.skipped_exports,
                                stdout: output.stdout,
                                stderr: output.stderr,
                                duration_ms: output.duration_ms,
                                cpu_time_ms: output.cpu_time_ms,
                                bridge_calls: output.bridge_calls,
                                heap_used_bytes: None,
                            }),
                        )
                    }
                    Err(failure) => {
                        trace!(
                            "[iso4-v8] run failed in {:.3}ms: {:?}",
                            failure.duration_ms,
                            failure.error
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
                                heap_used_bytes: None,
                            }),
                        )
                    }
                };
                shared.warm.release_oneoff();

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
                    payload.limits.memory_mb,
                ) {
                    Ok(()) => {
                        let prefix_id = shared
                            .next_prefix_id
                            .fetch_add(1, Ordering::Relaxed)
                            .to_string();
                        // Only bridge-backed globals are re-installable per run
                        // (string/data globals and shim wrappers are replayed
                        // from the stored prefix defs). The declared set is the
                        // bridge stub names a PrefixRun may re-bind; the
                        // undeclared-binding check validates against it.
                        let declared_globals: Vec<String> = payload
                            .globals
                            .iter()
                            .filter_map(|g| g.bridge_stub_name().map(str::to_string))
                            .collect();
                        eprintln!(
                            "[iso4-v8] precompile succeeded — prefix_id={prefix_id} \
                                 ({} source bytes, {} declared globals, {} declared imports)",
                            payload.code.len(),
                            declared_globals.len(),
                            payload.imports.len(),
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
                                Arc::new(PrefixData {
                                    code: payload.code,
                                    filename: payload.filename,
                                    globals: payload.globals,
                                    declared_globals,
                                    declared_imports: payload.imports,
                                }),
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

                // An `Arc` handle, so the store lock is held for one refcount
                // bump — not for a deep clone of the source and global defs on
                // every run of every slot.
                let prefix_data = shared
                    .prefix_store
                    .lock()
                    .unwrap_or_else(|p| p.into_inner()) // see poison comment above
                    .get(&payload.prefix_id)
                    .map(Arc::clone);

                let result_payload = match prefix_data {
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
                                heap_used_bytes: None,
                            }),
                        )
                    }
                    Some(prefix_data) => {
                        // ── ERR_UNDECLARED_BINDING check ─────────────────────────────────
                        // Every global in payload.globals must have been declared at
                        // precompile time. Adding a new name at run time would silently
                        // widen the global surface the prefix was validated against,
                        // breaking the invariant that the prefix declares the full
                        // bridge surface. The same rule covers host-import rebinds:
                        // only function leaves declared at precompile time may be
                        // re-pointed.
                        let declared_set: std::collections::HashSet<&str> = prefix_data
                            .declared_globals
                            .iter()
                            .map(String::as_str)
                            .collect();
                        let violation: Option<String> = payload
                            .globals
                            .iter()
                            .filter_map(|g| g.bridge_stub_name())
                            .find(|name| !declared_set.contains(name))
                            .map(|name| {
                                format!("global '{name}' was not declared at precompile time")
                            })
                            .or_else(|| {
                                validate_import_rebinds(
                                    &payload.import_rebinds,
                                    &prefix_data.declared_imports,
                                )
                                .err()
                            });
                        if let Some(msg) = violation {
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
                                    heap_used_bytes: None,
                                }),
                            )
                        } else {
                            // PrefixRun reuses the declared imports shape
                            // verbatim — rebinds only re-point host function
                            // handlers on the TS side (data exports + source
                            // modules are frozen at declaration), validated
                            // above. Every run global here is a bridge stub to
                            // re-install (string/data globals and shim
                            // wrappers are replayed from the stored prefix
                            // defs during prefix evaluation).
                            let stream_fd = if needs_bridge_stub(
                                &payload.globals,
                                &prefix_data.declared_imports,
                            ) {
                                Some(stream.as_raw_fd())
                            } else {
                                None
                            };
                            trace!(
                            "[iso4-v8] PrefixRun {} (prefix_id={}, {} code bytes, {} globals, {} imports, call={})",
                            payload.run_id, payload.prefix_id,
                            payload.code.as_deref().map(str::len).unwrap_or(0),
                            payload.globals.len(), prefix_data.declared_imports.len(),
                            payload.call.as_ref().map(|c| c.export_path.as_str()).unwrap_or("-"),
                        );
                            // ── Warm instance flow (#64/#66) ─────────────────
                            // Reuse the warmest idle instance of this prefix,
                            // or take a slot (shedding scored victims if the
                            // RSS watermark demands it) and cold-start a
                            // fresh one on its own owner thread. At the hard
                            // watermark the fresh instance is CreateCold: it
                            // serves this call and is dropped, never pooled —
                            // correctness never depends on warmth. The
                            // session thread parks on the response channel
                            // for the duration, so the socket has one user
                            // at a time.
                            let (handle, pooled) = match shared.warm.acquire(&payload.prefix_id) {
                                crate::warm::Acquired::Reused(h) => (h, true),
                                crate::warm::Acquired::CreateNew => (
                                    crate::warm::spawn_instance(
                                        Arc::clone(&prefix_data),
                                        payload.limits.memory_mb,
                                    ),
                                    true,
                                ),
                                crate::warm::Acquired::CreateCold => (
                                    crate::warm::spawn_instance(
                                        Arc::clone(&prefix_data),
                                        payload.limits.memory_mb,
                                    ),
                                    false,
                                ),
                            };
                            let outcome = handle.call(crate::warm::CallJob {
                                code: payload.code,
                                filename: payload.filename,
                                limits: sandbox::Limits {
                                    wall_time_ms: payload.limits.wall_time_ms,
                                    cpu_time_ms: payload.limits.cpu_time_ms,
                                    memory_mb: payload.limits.memory_mb,
                                    max_export_bytes: payload.limits.max_export_bytes,
                                    max_stdout_bytes: payload.limits.max_stdout_bytes,
                                    max_stderr_bytes: payload.limits.max_stderr_bytes,
                                    max_bridge_call_bytes: payload.limits.max_bridge_call_bytes,
                                    max_bridge_calls: payload.limits.max_bridge_calls,
                                },
                                globals: payload.globals,
                                stream_fd,
                                call_id_counter: Arc::clone(&call_id_counter),
                                call: payload.call,
                            });
                            // An instance of a since-disposed prefix must not
                            // return to the pool. The aliveness check and the
                            // release must be atomic w.r.t. DisposePrefix, or
                            // a dispose landing between them leaks the busy
                            // instance into the idle pool (dispose_prefix only
                            // sees idle instances, and this one is still busy).
                            // Both paths hold the prefix_store lock across the
                            // warm-registry call; the lock order is always
                            // prefix_store → warm, so no deadlock.
                            //
                            // A CreateCold instance never entered a pool, so
                            // the dispose race cannot touch it: drop the
                            // handle (the owner thread exits and disposes the
                            // isolate) and give back the one-off slot.
                            if pooled {
                                let store = shared
                                    .prefix_store
                                    .lock()
                                    .unwrap_or_else(|p| p.into_inner());
                                let prefix_alive = store.contains_key(&payload.prefix_id);
                                shared.warm.release(
                                    &payload.prefix_id,
                                    handle,
                                    outcome.tainted,
                                    outcome.heap_used_bytes,
                                    prefix_alive,
                                );
                            } else {
                                drop(handle);
                                shared.warm.release_oneoff();
                            }
                            match outcome.result {
                                Ok(output) => {
                                    trace!(
                                        "[iso4-v8] PrefixRun {} succeeded in {:.3}ms",
                                        payload.run_id,
                                        output.duration_ms
                                    );
                                    wire::encode_run_completion_payload(
                                        payload.run_id,
                                        wire::RunCompletion::Success(wire::RunSuccessPayload {
                                            exports: output.exports,
                                            skipped_exports: output.skipped_exports,
                                            stdout: output.stdout,
                                            stderr: output.stderr,
                                            duration_ms: output.duration_ms,
                                            cpu_time_ms: output.cpu_time_ms,
                                            bridge_calls: output.bridge_calls,
                                            heap_used_bytes: Some(outcome.heap_used_bytes),
                                        }),
                                    )
                                }
                                Err(failure) => {
                                    trace!(
                                        "[iso4-v8] PrefixRun {} failed in {:.3}ms: {:?}",
                                        payload.run_id,
                                        failure.duration_ms,
                                        failure.error
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
                                            heap_used_bytes: Some(outcome.heap_used_bytes),
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
                        // Hold the prefix_store lock across dispose_prefix so
                        // the remove and the idle-pool drop are atomic w.r.t.
                        // a concurrent release (see the release site). Busy
                        // instances are not in the idle pool yet; their
                        // release then observes the prefix gone and drops
                        // them instead of pooling them.
                        let existed = {
                            let mut store = shared
                                .prefix_store
                                .lock()
                                .unwrap_or_else(|p| p.into_inner());
                            let existed = store.remove(&prefix_id).is_some();
                            shared.warm.dispose_prefix(&prefix_id);
                            existed
                        };
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
            ipc::TsToRustMessageType::Stats => {
                // Capacity/usage snapshot (#65). Sent by the host's dedicated
                // control connection, so it never queues behind runs; the
                // payload is empty and the reply is one StatsResult frame.
                let stats = shared.warm.stats();
                if let Err(e) = ipc::write_rust_to_ts_frame(
                    &mut stream,
                    ipc::RustToTsMessageType::StatsResult,
                    &wire::encode_stats_payload(&stats),
                ) {
                    eprintln!("[iso4-v8] failed to write StatsResult frame: {e}");
                    break;
                }
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

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Drive `handle_client` over a socket pair with the given Authenticate
    /// payload and return the first frame it writes back (if any).
    fn handshake(payload: Vec<u8>, token: &str) -> Option<ipc::RustToTsFrame> {
        let (mut host, runtime) = UnixStream::pair().unwrap();
        let shared = Arc::new(SharedState::new(token.to_string(), 0));
        let server = std::thread::spawn(move || handle_client(runtime, shared));

        ipc::write_ts_to_rust_frame(&mut host, ipc::TsToRustMessageType::Authenticate, &payload)
            .unwrap();
        host.flush().unwrap();
        let frame = ipc::read_rust_to_ts_frame(&mut host).ok();
        drop(host);
        server.join().unwrap();
        frame
    }

    fn authenticate(probe: Vec<u8>, token: &str) -> Vec<u8> {
        ipc::encode_authenticate_payload(&ipc::AuthenticatePayload {
            protocol_version: ipc::PROTOCOL_VERSION,
            probe,
            token: token.to_string(),
        })
    }

    /// `status`, `probe`, `message` out of a `Hello` payload.
    fn parse_hello(payload: &[u8]) -> (u8, Vec<u8>, String) {
        let status = payload[0];
        let probe_len = u32::from_be_bytes(payload[1..5].try_into().unwrap()) as usize;
        let probe = payload[5..5 + probe_len].to_vec();
        let msg_start = 5 + probe_len + 4;
        let msg_len =
            u32::from_be_bytes(payload[5 + probe_len..msg_start].try_into().unwrap()) as usize;
        let message = String::from_utf8(payload[msg_start..msg_start + msg_len].to_vec()).unwrap();
        (status, probe, message)
    }

    #[test]
    fn valid_handshake_is_acknowledged_with_the_runtime_probe() {
        let frame = handshake(authenticate(crate::blob::probe().to_vec(), "tok"), "tok")
            .expect("expected a Hello frame");
        assert_eq!(frame.message_type, ipc::RustToTsMessageType::Hello);
        let (status, probe, message) = parse_hello(&frame.payload);
        assert_eq!(status, ipc::HelloStatus::Ok as u8);
        assert_eq!(probe, crate::blob::probe());
        assert_eq!(message, "");
    }

    #[test]
    fn impossible_v8_format_version_is_refused_loudly() {
        // A probe claiming a serialization format far newer than anything this
        // V8 can read. The old protocol closed the socket silently here.
        let probe = vec![crate::blob::V8_BLOB_HEADER_TAG, 0x63, 0x30];
        let frame = handshake(authenticate(probe, "tok"), "tok").expect("expected a Hello frame");
        let (status, _, message) = parse_hello(&frame.payload);
        assert_eq!(status, ipc::HelloStatus::V8FormatMismatch as u8);
        assert!(
            message.contains("V8 serialization format mismatch"),
            "unhelpful message: {message}"
        );
    }

    #[test]
    fn probe_that_is_not_a_blob_is_refused_loudly() {
        let frame = handshake(authenticate(vec![0x01, 0x02], "tok"), "tok")
            .expect("expected a Hello frame");
        let (status, _, message) = parse_hello(&frame.payload);
        assert_eq!(status, ipc::HelloStatus::V8FormatMismatch as u8);
        assert!(message.contains("unrecognised format"), "{message}");
    }

    #[test]
    fn protocol_version_mismatch_is_refused_loudly() {
        let payload = ipc::encode_authenticate_payload(&ipc::AuthenticatePayload {
            protocol_version: ipc::PROTOCOL_VERSION + 1,
            probe: crate::blob::probe().to_vec(),
            token: "tok".to_string(),
        });
        let frame = handshake(payload, "tok").expect("expected a Hello frame");
        let (status, _, message) = parse_hello(&frame.payload);
        assert_eq!(status, ipc::HelloStatus::ProtocolVersionMismatch as u8);
        assert!(message.contains("protocol version mismatch"), "{message}");
    }

    #[test]
    fn bad_token_closes_without_a_hello() {
        // Deliberately silent: an unauthenticated peer learns nothing.
        assert!(handshake(authenticate(crate::blob::probe().to_vec(), "wrong"), "tok").is_none());
    }
}
