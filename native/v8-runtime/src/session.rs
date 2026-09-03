//! Per-connection session handling.
//!
//! `handle_client` is the entry point for one authenticated connection. It
//! is the connection's DEMUX: the only socket reader, routing run-tagged
//! frames (BridgeResponse, stream frames, Terminate) by the leading run id
//! to the owning run's event channel — a one-off run's private channel, or
//! the owning warm instance's shared channel with the run's token as the
//! tag. Runs execute on worker threads (one-off `Run`s) or warm instance
//! threads (`PrefixRun`s) and write every outbound frame through ONE
//! serialized writer, so frames from concurrent runs never interleave
//! mid-frame.
//!
//! Blast radius: a corrupt or unparseable frame means framing can no longer
//! be trusted — the demux stops, every run in flight on THIS connection
//! fails cleanly (no instance taint: nothing was interrupted mid-JS), and
//! other connections are untouched.
//!
//! Protocol note: logs (stdout/stderr) are captured during execution and
//! returned inside the `Result` frame's `RunCompletionPayload`; there are no
//! `StdioChunk` frames.

use std::collections::HashMap;
use std::io::Read;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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
/// multi-isolate process).
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
    /// Warm instance registry: every PrefixRun is served by a resident
    /// isolate from here; one-off runs share the same slot accounting.
    pub warm: crate::warm::WarmRegistry,
}

impl SharedState {
    /// `warm_budget_bytes` is the mark the registry sheds against (global
    /// container usage; 0 disables it); `hard_line_bytes` is the isolate
    /// admission line (0 = no container limit readable). Concurrency is
    /// bounded by the host pool; there is no instance-count cap.
    pub fn new(warm_budget_bytes: u64, hard_line_bytes: u64) -> Self {
        Self {
            prefix_store: Mutex::new(HashMap::new()),
            next_prefix_id: AtomicU64::new(0),
            warm: crate::warm::WarmRegistry::new(warm_budget_bytes, hard_line_bytes),
        }
    }
}

/// How long a freshly accepted connection has to deliver its whole
/// `Authenticate` frame, measured from the first byte of the length prefix.
///
/// The host writes that frame immediately after `connect()`, so this is orders
/// of magnitude more time than the handshake actually takes; it exists so that
/// a connection which never gets around to authenticating stops occupying a
/// thread. Tests use a short deadline so the suite does not wait on it.
#[cfg(not(test))]
const HANDSHAKE_DEADLINE: Duration = Duration::from_secs(2);
#[cfg(test)]
const HANDSHAKE_DEADLINE: Duration = Duration::from_millis(200);

/// Reader that bounds the handshake as a whole rather than each read inside it.
///
/// A plain `set_read_timeout` is not enough: the socket timer restarts on every
/// successful read, so a peer that sends one byte per interval keeps its
/// connection, and its thread, for as long as it cares to. This computes the
/// time left before each read instead and hands that to the socket, so the
/// budget is spent whether the peer is silent or merely slow. `read_exact` in
/// the frame reader loops over `read`, so a partial frame is covered too.
///
/// Used for the `Authenticate` frame only. The deadline is dropped once the
/// peer is authenticated, because an idle connection is normal after that —
/// the host's pool holds connections open between runs and they may wait
/// indefinitely for the next one.
struct HandshakeReader<'a> {
    stream: &'a mut UnixStream,
    deadline: Instant,
}

impl Read for HandshakeReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let left = self.deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "handshake deadline passed",
            ));
        }
        self.stream.set_read_timeout(Some(left))?;
        self.stream.read(buf)
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

/// Write a `Result` frame, substituting a minimal one when the assembled
/// payload is too large to frame.
///
/// `max_export_bytes` bounds the *success* payload, but nothing bounds a
/// failure: the error's message, stack and own properties are cloned verbatim
/// out of the isolate, so `throw new Error('A'.repeat(70e6))` builds a payload
/// past the 64 MiB frame ceiling under default limits. Writing it used to fail,
/// and the caller treated that as fatal and closed the connection with no frame
/// sent at all — which the host then recycled, one dead pool slot per run, until
/// the sandbox served nothing. Guest code could do that deliberately.
///
/// Substituting is safe because the length check in `write_frame_with_limit`
/// runs before any byte reaches the socket, so the stream is still frame-aligned
/// and the run can be answered properly. Only a genuine I/O failure (a
/// different `ErrorKind`) may have written a partial frame, and that stays
/// fatal (the demux observes the dead socket on its next read).
fn write_result_frame(sink: &ipc::FrameSink, run_id: u32, payload: Vec<u8>) -> std::io::Result<()> {
    match sink.write(ipc::RustToTsMessageType::Result, payload) {
        Err(e) if e.kind() == std::io::ErrorKind::InvalidInput => {
            eprintln!(
                "[iso4-v8] Result frame for run {run_id} too large to send ({e}) — \
                 replacing it with ERR_INTERNAL"
            );
            let replacement = wire::encode_run_completion_payload(
                run_id,
                wire::RunCompletion::Failure(wire::RunFailurePayload {
                    error: wire::RunErrorPayload {
                        code: "ERR_INTERNAL".to_string(),
                        name: "Error".to_string(),
                        message: format!(
                            "the run's result was too large to send to the host ({e}). \
                             A thrown error's message, stack and properties are reported \
                             verbatim, so a very large one cannot be delivered."
                        ),
                        stack: None,
                        fields: None,
                        reset: None,
                    },
                    // Dropped deliberately: stdout/stderr are unbounded too, and
                    // this payload has to be small by construction.
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                    duration_ms: 0.0,
                    cpu_time_ms: 0.0,
                    bridge_calls: Vec::new(),
                    heap_used_bytes: None,
                }),
            );
            sink.write(ipc::RustToTsMessageType::Result, replacement)
        }
        other => other,
    }
}

/// Write a `RunComplete` frame, substituting a minimal one when the
/// grace report is too large to frame — same recovery `write_result_frame`
/// gives Result frames, so a guest-authored oversized rejection message (or
/// an unbounded record list under `maxBridgeCalls: 0`) costs its telemetry,
/// never the connection. The status byte survives (payload offset 4).
fn write_run_complete_frame(
    sink: &ipc::FrameSink,
    run_id: u32,
    payload: Vec<u8>,
) -> std::io::Result<()> {
    // Captured before the payload moves into the send — the substitute path
    // below needs it after a refused write.
    let status_byte = payload.get(4).copied().unwrap_or(1);
    match sink.write(ipc::RustToTsMessageType::RunComplete, payload) {
        Err(e) if e.kind() == std::io::ErrorKind::InvalidInput => {
            eprintln!(
                "[iso4-v8] RunComplete frame for run {run_id} too large to send ({e}) — \
                 substituting a minimal report"
            );
            let minimal = wire::encode_minimal_run_complete(run_id, status_byte);
            sink.write(ipc::RustToTsMessageType::RunComplete, minimal)
        }
        other => other,
    }
}

/// The failure a run refused by the memory admission reports: nothing ran,
/// so every telemetry field is genuinely zero.
fn capacity_failure(refusal: String) -> sandbox::FailureOutput {
    sandbox::FailureOutput {
        error: sandbox::RunError::Capacity(refusal),
        stdout: Vec::new(),
        stderr: Vec::new(),
        duration_ms: 0.0,
        cpu_time_ms: 0.0,
        bridge_calls: Vec::new(),
    }
}

/// Encode and write a run's completion (the final Result, or the RunComplete
/// when the early Result already shipped during grace). Shared by the
/// one-off worker and the warm completion hook. Write failures are logged:
/// the demux observes the dead socket on its own next read.
fn write_completion(
    sink: &ipc::FrameSink,
    run_id: u32,
    outcome_result: &Result<sandbox::Output, sandbox::FailureOutput>,
    heap_used_bytes: Option<u64>,
) {
    let (frame_is_run_complete, payload) = match outcome_result {
        Ok(output) => match &output.background {
            // The run flow already wrote the early Result and drove the
            // grace phase; what remains is the final RunComplete frame.
            Some(bg) if bg.early_result_sent => {
                (true, wire::encode_run_complete_payload(run_id, bg))
            }
            _ => (
                false,
                wire::encode_run_completion_payload(
                    run_id,
                    wire::RunCompletion::Success(wire::RunSuccessPayload {
                        exports: output.exports.clone(),
                        skipped_exports: output.skipped_exports.clone(),
                        stdout: output.stdout.clone(),
                        stderr: output.stderr.clone(),
                        duration_ms: output.duration_ms,
                        cpu_time_ms: output.cpu_time_ms,
                        bridge_calls: output.bridge_calls.clone(),
                        heap_used_bytes,
                        background_pending: false,
                    }),
                ),
            ),
        },
        Err(failure) => (
            false,
            wire::encode_run_completion_payload(
                run_id,
                wire::RunCompletion::Failure(wire::RunFailurePayload {
                    error: wire::run_error_to_payload(&failure.error),
                    stdout: failure.stdout.clone(),
                    stderr: failure.stderr.clone(),
                    duration_ms: failure.duration_ms,
                    cpu_time_ms: failure.cpu_time_ms,
                    bridge_calls: failure.bridge_calls.clone(),
                    heap_used_bytes,
                }),
            ),
        ),
    };
    let write_outcome = if frame_is_run_complete {
        write_run_complete_frame(sink, run_id, payload)
    } else {
        write_result_frame(sink, run_id, payload)
    };
    if let Err(e) = write_outcome {
        eprintln!("[iso4-v8] failed to write completion frame for run {run_id}: {e}");
    }
}

/// Where the demux routes one in-flight run's inbound frames: a one-off
/// run's private channel, or the owning warm instance's shared event
/// channel. Every event is tagged with the run's `token` so the consumer
/// delivers it to the right run.
struct RunRoute {
    frames: sandbox::RunEventSender,
    /// Per-run inbound frame cap (the run's memory budget) — demux policy.
    frame_cap: u32,
    /// The run's instance-local token: the event tag, and the mid-turn
    /// abort target.
    token: u64,
    /// The owning instance guard's cross-thread face, filled once the
    /// isolate exists.
    ctl: std::sync::Arc<std::sync::OnceLock<sandbox::GuardCtl>>,
}

/// This connection's in-flight runs, shared between the demux reader and the
/// completion hooks (which remove their run when it finishes).
type ConnRuns = Arc<Mutex<HashMap<u32, RunRoute>>>;

/// The per-run inbound frame allowance, enforced during routing (a frame
/// over it fails that run alone). The DEMUX read itself uses the flat
/// protocol ceiling instead: the host's encoder refuses to emit any frame
/// over `DEFAULT_MAX_FRAME_LENGTH`, so an allowance above it is
/// unreachable, and recomputing a max over live runs per inbound frame
/// (with a lock) bought nothing. The constant ceiling also never shrinks
/// under an in-flight frame, so a late big frame for a just-completed run
/// reads fine and is discarded as late — only a frame the host could never
/// have sent kills the connection.
fn frame_cap_for(memory_mb: u32) -> u32 {
    if memory_mb > 0 {
        memory_mb.saturating_mul(1024 * 1024)
    } else {
        ipc::DEFAULT_MAX_FRAME_LENGTH
    }
}

pub fn handle_client(mut stream: UnixStream, shared: Arc<SharedState>) {
    // ── Step 1 & 2: authenticate ──────────────────────────────────────────

    // The first frame comes from a peer that has shown nothing yet, so it is
    // read on a deadline and against a ceiling of its own (`AUTH_MAX_FRAME_LENGTH`)
    // rather than the one sized for run payloads.
    let auth_frame = {
        let mut reader = HandshakeReader {
            stream: &mut stream,
            deadline: Instant::now() + HANDSHAKE_DEADLINE,
        };
        match ipc::read_ts_to_rust_frame_with_limit(&mut reader, ipc::AUTH_MAX_FRAME_LENGTH) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("[iso4-v8] failed to read first frame: {e}");
                return;
            }
        }
    };

    // Authenticated peers may idle: the host's pool keeps connections open
    // between runs, so every read after this one blocks for as long as it
    // takes. Failing to clear the deadline would put the handshake limit on an
    // idle connection, so it is fatal rather than best-effort.
    if let Err(e) = stream.set_read_timeout(None) {
        eprintln!("[iso4-v8] failed to clear the handshake deadline: {e} — closing");
        return;
    }

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
    //
    // Read and write are split on purpose: peer probes are accepted up to the
    // version this V8 READS (its native format), while every blob this binary
    // emits — the Hello probe included — carries the relabelled WRITE version
    // (see `blob::relabel_write_version`). One binary spans Node lines on
    // either side of V8's 15→16 format bump.
    let host_version = crate::blob::probe_format_version(&auth.probe);
    let read_version = crate::blob::read_format_version();
    match host_version {
        Some(v) if v <= read_version => {}
        _ => {
            let message = format!(
                "V8 serialization format mismatch between Node (writes format {}) and the \
                 iso4-v8 binary (reads up to format {read_version}). \
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

    if let Err(e) = ipc::write_rust_to_ts_frame(
        &mut stream,
        ipc::RustToTsMessageType::Hello,
        &ipc::encode_hello_payload(ipc::HelloStatus::Ok, crate::blob::probe(), ""),
    ) {
        eprintln!("[iso4-v8] failed to write Hello frame: {e}");
        return;
    }

    // Brand key for host-emitted host-type descriptors (docs/protocol.md
    // §4.4.6): derived from the handshake token, installed for this session
    // thread (one-off runs, precompile) and handed to every warm instance this
    // connection spawns. Only descriptors stamped with it rehydrate.
    let brand_key = crate::webcodec::brand_key_for_token(&auth.descriptor_token);
    crate::webcodec::set_session_brand_key(brand_key.clone());

    eprintln!("[iso4-v8] handshake complete");

    // Per-connection bridge call-ID counter.  Monotonically increasing across
    // all runs on this connection so callIds never reset to 0 between runs;
    // with several runs in flight the counter also keeps their callIds
    // disjoint.
    let call_id_counter = Arc::new(AtomicU32::new(0));

    // ── Step 3: the demux loop ────────────────────────────────────────────
    //
    // This thread is the connection's only reader. Run-tagged frames
    // (BridgeResponse, stream frames, Terminate) route to the owning run's
    // channel; runs execute on worker/instance threads and write their
    // outbound frames through ONE serialized writer, so frames from
    // concurrent runs never interleave mid-frame. Control frames
    // (Precompile, DisposePrefix, Stats) are handled here or on short-lived
    // workers.
    let outbox = match ipc::Outbox::spawn(&stream) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("[iso4-v8] failed to start the connection writer: {e} — closing");
            return;
        }
    };
    let sink = ipc::FrameSink::Shared(outbox);
    let conn_runs: ConnRuns = Arc::new(Mutex::new(HashMap::new()));

    // ── Teardown guard ────────────────────────────────────────────────────
    // When the demux ends — EOF, I/O error, protocol violation, or a panic
    // unwinding this thread — framing can no longer be trusted: every run in
    // flight on THIS connection fails cleanly and releases its slot;
    // instances are NOT tainted (nothing was interrupted mid-JS) and other
    // connections are unaffected — the epic's blast-radius bound. A Drop
    // guard rather than code after the loop because a warm instance's event
    // channel is shared and outlives any one connection, so a panicked demux
    // dropping its routes would otherwise strand runs waiting on it forever.
    // The outbox needs no explicit close: it drains and exits when the last
    // sink clone drops — a run concluding after this teardown still gets its
    // failure Result out, exactly like the old direct-write socket clone.
    struct FailRunsOnDrop(ConnRuns);
    impl Drop for FailRunsOnDrop {
        fn drop(&mut self) {
            let routes: Vec<RunRoute> = {
                let mut runs = self.0.lock().unwrap_or_else(|p| p.into_inner());
                runs.drain().map(|(_, r)| r).collect()
            };
            for route in routes {
                route
                    .frames
                    .send(sandbox::RoutedEvent::new(
                        route.token,
                        sandbox::RunEvent::ConnLost(None),
                    ))
                    .ok();
            }
        }
    }
    let _teardown = FailRunsOnDrop(Arc::clone(&conn_runs));

    loop {
        let frame = match ipc::read_ts_to_rust_frame_with_limit(
            &mut stream,
            ipc::DEFAULT_MAX_FRAME_LENGTH,
        ) {
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
                dispatch_oneoff_run(
                    payload,
                    &shared,
                    &sink,
                    &conn_runs,
                    &call_id_counter,
                    &brand_key,
                );
            }
            ipc::TsToRustMessageType::Precompile => {
                let payload = match ipc::parse_precompile_payload(&frame.payload) {
                    Ok(p) => p,
                    Err(e) => {
                        eprintln!("[iso4-v8] malformed Precompile payload: {e} — closing");
                        break;
                    }
                };
                dispatch_precompile(payload, &shared, &sink, &brand_key);
            }
            ipc::TsToRustMessageType::PrefixRun => {
                let payload = match ipc::parse_prefix_run_payload(&frame.payload) {
                    Ok(p) => p,
                    Err(e) => {
                        eprintln!("[iso4-v8] malformed PrefixRun payload: {e} — closing");
                        break;
                    }
                };
                dispatch_prefix_run(
                    payload,
                    &shared,
                    &sink,
                    &conn_runs,
                    &call_id_counter,
                    &brand_key,
                );
            }
            ipc::TsToRustMessageType::DisposePrefix => {
                match ipc::parse_dispose_prefix_payload(&frame.payload) {
                    Ok(prefix_id) => {
                        // Hold the prefix_store lock across dispose_prefix so
                        // the remove and the idle-pool drop are atomic w.r.t.
                        // a concurrent release (see the release hook). Busy
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
            ipc::TsToRustMessageType::Stats => {
                // Capacity/usage snapshot. Sent by the host's dedicated
                // control connection, so it never queues behind runs; the
                // payload is empty and the reply is one StatsResult frame.
                let stats = shared.warm.stats();
                if let Err(e) = sink.write(
                    ipc::RustToTsMessageType::StatsResult,
                    wire::encode_stats_payload(&stats),
                ) {
                    eprintln!("[iso4-v8] failed to write StatsResult frame: {e}");
                    break;
                }
            }
            ipc::TsToRustMessageType::BridgeResponse
            | ipc::TsToRustMessageType::StreamChunk
            | ipc::TsToRustMessageType::StreamEnd => {
                if let Err(e) = route_run_frame(&conn_runs, frame) {
                    eprintln!("[iso4-v8] {e} — closing");
                    break;
                }
            }
            ipc::TsToRustMessageType::Terminate => {
                if let Err(e) = route_terminate(&conn_runs, frame) {
                    eprintln!("[iso4-v8] {e} — closing");
                    break;
                }
            }
        }
    }

    // Teardown happens in `FailRunsOnDrop` above — shared by the normal exit
    // here and a panic unwind.
}

/// Route one run-tagged frame (BridgeResponse / stream frames) to its run.
/// Every such payload leads with the wire run id. Unknown runs get the
/// late-frame discard (the run completed while the frame was in flight);
/// a frame over its run's inbound allowance fails that run alone. A payload
/// too short to carry a run id is CORRUPT — `Err` closes the connection
/// (the blast-radius rule: unparseable framing fails this connection's runs
/// loudly rather than leaving one waiting forever).
fn route_run_frame(conn_runs: &ConnRuns, frame: ipc::TsToRustFrame) -> Result<(), String> {
    let Some(run_id) = peek_run_id(&frame.payload) else {
        return Err(format!(
            "truncated {:?} frame (no run id)",
            frame.message_type
        ));
    };
    let runs = conn_runs.lock().unwrap_or_else(|p| p.into_inner());
    let Some(route) = runs.get(&run_id) else {
        match frame.message_type {
            ipc::TsToRustMessageType::BridgeResponse => {
                eprintln!("[iso4-v8] ignoring late BridgeResponse (run already completed)")
            }
            _ => eprintln!("[iso4-v8] ignoring late stream frame (run already completed)"),
        }
        return Ok(());
    };
    // Per-run inbound cap — demux policy (the read itself is bounded by the
    // connection-wide ceiling; this enforces the RUN's own allowance).
    let frame_len = frame.payload.len().saturating_add(1);
    if frame_len > route.frame_cap as usize {
        let cap = route.frame_cap;
        route
            .frames
            .send(sandbox::RoutedEvent::new(
                route.token,
                sandbox::RunEvent::ConnLost(Some(format!(
                    "frame length {frame_len} exceeds max frame length {cap}"
                ))),
            ))
            .ok();
        return Ok(());
    }
    route
        .frames
        .send(sandbox::RoutedEvent::new(
            route.token,
            sandbox::RunEvent::Frame(frame),
        ))
        .ok();
    Ok(())
}

/// Route a Terminate: if the target run's own turn is executing right now,
/// terminate it mid-turn (the taint fallback — E1 ruling 5); otherwise the
/// routed frame abandons the suspended run cleanly at its next event.
fn route_terminate(conn_runs: &ConnRuns, frame: ipc::TsToRustFrame) -> Result<(), String> {
    let run_id = match ipc::parse_terminate_payload(&frame.payload) {
        Ok(id) => id,
        // A Terminate that cannot be parsed is corrupt framing — close the
        // connection rather than leaving the (unidentifiable) target running.
        Err(e) => return Err(format!("malformed Terminate payload: {e}")),
    };
    let runs = conn_runs.lock().unwrap_or_else(|p| p.into_inner());
    let Some(route) = runs.get(&run_id) else {
        // The run completed just before the abort landed — a benign race.
        // Discard and keep the connection healthy for its other runs.
        eprintln!("[iso4-v8] ignoring stray Terminate (run {run_id} already completed)");
        return Ok(());
    };
    if let Some(ctl) = route.ctl.get() {
        if ctl.abort_executing(route.token) {
            eprintln!("[iso4-v8] Terminate for run {run_id} landed mid-turn — terminating");
            // The loop classifies the kill and resets the instance; no
            // frame delivery needed (the run's channel dies with it).
            return Ok(());
        }
    }
    route
        .frames
        .send(sandbox::RoutedEvent::new(
            route.token,
            sandbox::RunEvent::Frame(frame),
        ))
        .ok();
    Ok(())
}

/// The leading `u32` of a run-tagged payload.
fn peek_run_id(payload: &[u8]) -> Option<u32> {
    payload
        .get(0..4)
        .map(|b| u32::from_be_bytes(b.try_into().expect("4-byte slice")))
}

/// Whether a run id is already in flight on this connection. Replacing a
/// live route would strand the first run (its frames discarded as late) and
/// misdeliver its completion cleanup, so a duplicate is refused loudly
/// BEFORE anything is acquired for the new run. The demux thread is this
/// connection's only route inserter, so a clear check here stays true
/// through the dispatch that follows.
fn run_id_in_flight(conn_runs: &ConnRuns, run_id: u32) -> bool {
    conn_runs
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .contains_key(&run_id)
}

/// Register a run with the demux: events route to `frames`, tagged with
/// `token`. The caller checked [`run_id_in_flight`] first.
fn insert_run_route(
    conn_runs: &ConnRuns,
    run_id: u32,
    token: u64,
    memory_mb: u32,
    ctl: Arc<std::sync::OnceLock<sandbox::GuardCtl>>,
    frames: sandbox::RunEventSender,
) {
    let mut runs = conn_runs.lock().unwrap_or_else(|p| p.into_inner());
    let replaced = runs.insert(
        run_id,
        RunRoute {
            frames,
            frame_cap: frame_cap_for(memory_mb),
            token,
            ctl,
        },
    );
    debug_assert!(replaced.is_none(), "run id checked before dispatch");
}

/// Answer a run the demux refused before dispatch (duplicate run id).
fn refuse_duplicate_run(sink: &ipc::FrameSink, run_id: u32) {
    eprintln!(
        "[iso4-v8] run {run_id} is already in flight on this connection — refusing the duplicate"
    );
    let payload = wire::encode_run_completion_payload(
        run_id,
        wire::RunCompletion::Failure(wire::RunFailurePayload {
            error: wire::RunErrorPayload {
                code: "ERR_INTERNAL".to_string(),
                name: "Error".to_string(),
                message: format!(
                    "run id {run_id} is already in flight on this connection; every \
                     concurrent run needs a distinct id"
                ),
                stack: None,
                fields: None,
                reset: None,
            },
            stdout: Vec::new(),
            stderr: Vec::new(),
            duration_ms: 0.0,
            cpu_time_ms: 0.0,
            bridge_calls: Vec::new(),
            heap_used_bytes: None,
        }),
    );
    if let Err(e) = write_result_frame(sink, run_id, payload) {
        eprintln!("[iso4-v8] failed to write completion frame: {e}");
    }
}

/// A one-off `Run`: executed on its own worker thread (a fresh isolate every
/// time), off the session thread so the demux keeps routing while it runs.
fn dispatch_oneoff_run(
    payload: ipc::RunPayload,
    shared: &Arc<SharedState>,
    sink: &ipc::FrameSink,
    conn_runs: &ConnRuns,
    call_id_counter: &Arc<AtomicU32>,
    brand_key: &str,
) {
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

    let run_id = payload.run_id;
    if run_id_in_flight(conn_runs, run_id) {
        refuse_duplicate_run(sink, run_id);
        return;
    }
    // Fresh isolate per one-off, shared ledger; admission-checked before
    // the route exists so a refusal leaves nothing to unwind.
    let run_cap_bytes = u64::from(payload.limits.memory_mb) * 1024 * 1024;
    if let Err(refusal) = shared.warm.reserve_oneoff(run_cap_bytes) {
        write_completion(sink, run_id, &Err(capacity_failure(refusal)), None);
        return;
    }

    let token = sandbox::alloc_run_token();
    let ctl = Arc::new(std::sync::OnceLock::new());
    // A one-off run gets a private event channel (it owns no instance loop).
    let (events_tx, events) = crossbeam_channel::unbounded();
    insert_run_route(
        conn_runs,
        run_id,
        token,
        payload.limits.memory_mb,
        Arc::clone(&ctl),
        events_tx,
    );

    let worker_shared = Arc::clone(shared);
    let worker_sink = sink.clone();
    let worker_conn_runs = Arc::clone(conn_runs);
    let call_id_counter = Arc::clone(call_id_counter);
    let brand_key = brand_key.to_string();
    let spawned = std::thread::Builder::new()
        .name("iso4-oneoff-run".to_string())
        .spawn(move || {
            let (shared, sink, conn_runs) = (worker_shared, worker_sink, worker_conn_runs);
            crate::webcodec::set_session_brand_key(brand_key);
            sandbox::set_run_epilogue_spec(Some(sandbox::EpilogueSpec {
                run_id,
                report_heap: false,
            }));
            let result = sandbox::execute_with_io(
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
                    grace_ms: payload.limits.grace_ms,
                },
                &payload.globals,
                &payload.imports,
                sandbox::RunIo::Session {
                    events,
                    sink: sink.clone(),
                },
                token,
                call_id_counter,
                payload.call.as_ref(),
                Some(ctl),
            );
            match &result {
                Ok(output) => {
                    trace!("[iso4-v8] run succeeded in {:.3}ms", output.duration_ms)
                }
                Err(failure) => trace!(
                    "[iso4-v8] run failed in {:.3}ms: {:?}",
                    failure.duration_ms,
                    failure.error
                ),
            }
            conn_runs
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&run_id);
            shared.warm.release_oneoff();
            write_completion(&sink, run_id, &result, None);
        });
    if let Err(e) = spawned {
        eprintln!("[iso4-v8] failed to spawn one-off worker: {e}");
        conn_runs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&run_id);
        shared.warm.release_oneoff();
        let failure = sandbox::FailureOutput {
            error: sandbox::RunError::Internal(format!("failed to spawn run worker: {e}")),
            stdout: Vec::new(),
            stderr: Vec::new(),
            duration_ms: 0.0,
            cpu_time_ms: 0.0,
            bridge_calls: Vec::new(),
        };
        write_completion(sink, run_id, &Err(failure), None);
    }
}

/// A `Precompile`: validated on a short-lived worker so a slow prefix does
/// not stall the demux; the result frame goes through the shared writer.
fn dispatch_precompile(
    payload: ipc::PrecompilePayload,
    shared: &Arc<SharedState>,
    sink: &ipc::FrameSink,
    brand_key: &str,
) {
    let shared = Arc::clone(shared);
    let worker_sink = sink.clone();
    let brand_key = brand_key.to_string();
    let request_id = payload.request_id;
    let spawned = std::thread::Builder::new()
        .name("iso4-precompile".to_string())
        .spawn(move || {
            let sink = worker_sink;
            crate::webcodec::set_session_brand_key(brand_key);
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
                    wire::encode_precompile_result_payload(request_id, Some(&prefix_id), None)
                }
                Err(failure) => {
                    eprintln!(
                        "[iso4-v8] precompile failed in {:.3}ms: {:?}",
                        failure.duration_ms, failure.error
                    );
                    wire::encode_precompile_result_payload(
                        request_id,
                        None,
                        Some(&wire::run_error_to_payload(&failure.error)),
                    )
                }
            };
            if let Err(e) = sink.write(ipc::RustToTsMessageType::PrecompileResult, result_payload)
            {
                eprintln!("[iso4-v8] failed to write PrecompileResult frame: {e}");
            }
        });
    if let Err(e) = spawned {
        eprintln!("[iso4-v8] failed to spawn precompile worker: {e}");
        let payload = wire::encode_precompile_result_payload(
            request_id,
            None,
            Some(&wire::run_error_to_payload(&sandbox::RunError::Internal(
                format!("failed to spawn precompile worker: {e}"),
            ))),
        );
        if let Err(e) = sink.write(ipc::RustToTsMessageType::PrecompileResult, payload) {
            eprintln!("[iso4-v8] failed to write PrecompileResult frame: {e}");
        }
    }
}

/// A `PrefixRun`: validated here (cheap map lookups), then dispatched to a
/// warm instance whose completion hook writes the frames and releases the
/// registry state — the demux never parks.
fn dispatch_prefix_run(
    payload: ipc::PrefixRunPayload,
    shared: &Arc<SharedState>,
    sink: &ipc::FrameSink,
    conn_runs: &ConnRuns,
    call_id_counter: &Arc<AtomicU32>,
    brand_key: &str,
) {
    let run_id = payload.run_id;

    // An `Arc` handle, so the store lock is held for one refcount bump — not
    // for a deep clone of the source and global defs on every run.
    let prefix_data = shared
        .prefix_store
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(&payload.prefix_id)
        .map(Arc::clone);

    let Some(prefix_data) = prefix_data else {
        eprintln!(
            "[iso4-v8] PrefixRun {} — prefix_id={} not found",
            run_id, payload.prefix_id
        );
        let payload = wire::encode_run_completion_payload(
            run_id,
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
                    reset: None,
                },
                stdout: Vec::new(),
                stderr: Vec::new(),
                duration_ms: 0.0,
                cpu_time_ms: 0.0,
                bridge_calls: Vec::new(),
                heap_used_bytes: None,
            }),
        );
        if let Err(e) = write_result_frame(sink, run_id, payload) {
            eprintln!("[iso4-v8] failed to write completion frame: {e}");
        }
        return;
    };

    // ── ERR_UNDECLARED_BINDING check ─────────────────────────────────────
    // Every global in payload.globals must have been declared at precompile
    // time; the same rule covers host-import rebinds.
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
        .map(|name| format!("global '{name}' was not declared at precompile time"))
        .or_else(|| {
            validate_import_rebinds(&payload.import_rebinds, &prefix_data.declared_imports).err()
        });
    if let Some(msg) = violation {
        eprintln!("[iso4-v8] PrefixRun {run_id} — ERR_UNDECLARED_BINDING: {msg}");
        let payload = wire::encode_run_completion_payload(
            run_id,
            wire::RunCompletion::Failure(wire::RunFailurePayload {
                error: wire::RunErrorPayload {
                    code: "ERR_UNDECLARED_BINDING".to_string(),
                    name: "Error".to_string(),
                    message: msg,
                    stack: None,
                    fields: None,
                    reset: None,
                },
                stdout: Vec::new(),
                stderr: Vec::new(),
                duration_ms: 0.0,
                cpu_time_ms: 0.0,
                bridge_calls: Vec::new(),
                heap_used_bytes: None,
            }),
        );
        if let Err(e) = write_result_frame(sink, run_id, payload) {
            eprintln!("[iso4-v8] failed to write completion frame: {e}");
        }
        return;
    }

    trace!(
        "[iso4-v8] PrefixRun {} (prefix_id={}, {} code bytes, {} globals, {} imports, call={})",
        run_id,
        payload.prefix_id,
        payload.code.as_deref().map(str::len).unwrap_or(0),
        payload.globals.len(),
        prefix_data.declared_imports.len(),
        payload
            .call
            .as_ref()
            .map(|c| c.export_path.as_str())
            .unwrap_or("-"),
    );

    // Refuse a duplicate before anything is acquired — nothing to release.
    // (The route itself is inserted after acquisition, because it points at
    // the acquired instance's event channel; the demux thread is the only
    // inserter, so the check holds through the dispatch below.)
    if run_id_in_flight(conn_runs, run_id) {
        refuse_duplicate_run(sink, run_id);
        return;
    }
    let token = sandbox::alloc_run_token();
    let ctl = Arc::new(std::sync::OnceLock::new());

    // ── Warm instance flow ───────────────────────────────────────────────
    // The registry decides (#77): reuse idle, join a busy instance, or
    // spawn — Cold = shedding with nothing to join; Refused = the
    // admission line.
    let spawn = || {
        crate::warm::spawn_instance(
            Arc::clone(&prefix_data),
            payload.limits.memory_mb,
            brand_key.to_string(),
        )
    };
    let run_cap_bytes = u64::from(payload.limits.memory_mb) * 1024 * 1024;
    enum Taken {
        Pooled(crate::warm::AttachedInstance),
        Cold(crate::warm::InstanceHandle),
    }
    let taken = match shared.warm.acquire(&payload.prefix_id, run_cap_bytes, &spawn) {
        crate::warm::Acquired::Attached(att) => Taken::Pooled(att),
        crate::warm::Acquired::Cold(handle) => Taken::Cold(handle),
        crate::warm::Acquired::Refused(refusal) => {
            // No registry state taken, no route inserted — just answer.
            write_completion(sink, run_id, &Err(capacity_failure(refusal)), None);
            return;
        }
        crate::warm::Acquired::SpawnFailed(e) => {
            eprintln!("[iso4-v8] PrefixRun {run_id} — failed to spawn instance thread: {e}");
            let failure = sandbox::FailureOutput {
                error: sandbox::RunError::Internal(format!(
                    "failed to spawn instance thread: {e}"
                )),
                stdout: Vec::new(),
                stderr: Vec::new(),
                duration_ms: 0.0,
                cpu_time_ms: 0.0,
                bridge_calls: Vec::new(),
            };
            write_completion(sink, run_id, &Err(failure), None);
            return;
        }
    };
    let (jobs_tx, events_tx) = match &taken {
        Taken::Pooled(att) => (att.sender(), att.event_sender()),
        Taken::Cold(handle) => (handle.sender(), handle.event_sender()),
    };

    // Route the run to the acquired instance's event channel; from here the
    // demux can deliver frames, aborts and connection loss by token.
    insert_run_route(
        conn_runs,
        run_id,
        token,
        payload.limits.memory_mb,
        Arc::clone(&ctl),
        events_tx,
    );

    // The completion hook, run on the instance thread when the run finishes:
    // release the instance FIRST (so the host's next run on this prefix
    // reuses it deterministically), then write the completion frames.
    // An instance of a since-disposed prefix must not return to the pool:
    // the aliveness check and the release are atomic w.r.t. DisposePrefix
    // under the prefix_store lock (lock order prefix_store → warm).
    let complete: sandbox::CompletionHook = {
        let shared = Arc::clone(shared);
        let conn_runs = Arc::clone(conn_runs);
        let sink = sink.clone();
        let prefix_id = payload.prefix_id.clone();
        Box::new(move |outcome: sandbox::CallOutcome| {
            conn_runs
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&run_id);
            match taken {
                Taken::Pooled(att) => {
                    let store = shared
                        .prefix_store
                        .lock()
                        .unwrap_or_else(|p| p.into_inner());
                    let prefix_alive = store.contains_key(&prefix_id);
                    let cpu_time_ms = match &outcome.result {
                        Ok(output) => output.cpu_time_ms,
                        Err(failure) => failure.cpu_time_ms,
                    };
                    shared.warm.release(
                        &prefix_id,
                        att.id,
                        outcome.tainted,
                        outcome.heap_used_bytes,
                        cpu_time_ms,
                        prefix_alive,
                    );
                }
                Taken::Cold(handle) => {
                    // Never pooled: drop the handle (the owner thread exits
                    // and disposes the isolate), give back the one-off slot.
                    drop(handle);
                    shared.warm.release_oneoff();
                }
            }
            match &outcome.result {
                Ok(output) => trace!(
                    "[iso4-v8] PrefixRun {} succeeded in {:.3}ms",
                    run_id,
                    output.duration_ms
                ),
                Err(failure) => trace!(
                    "[iso4-v8] PrefixRun {} failed in {:.3}ms: {:?}",
                    run_id,
                    failure.duration_ms,
                    failure.error
                ),
            }
            write_completion(&sink, run_id, &outcome.result, Some(outcome.heap_used_bytes));
        })
    };

    let job = Box::new(sandbox::CallJob {
        token,
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
            grace_ms: payload.limits.grace_ms,
        },
        globals: payload.globals,
        io: sandbox::RunIo::Instance { sink: sink.clone() },
        call_id_counter: Arc::clone(call_id_counter),
        call: payload.call,
        epilogue: Some(sandbox::EpilogueSpec {
            run_id,
            report_heap: true,
        }),
        complete: Some(complete),
        ctl_slot: Some(ctl),
    });

    if let Err((mut job, _)) = jobs_tx.send((job, None)).map_err(|e| e.0) {
        // The instance thread died before accepting the job: answer the run
        // through the completion hook, which also releases the registry
        // state it captured.
        eprintln!("[iso4-v8] PrefixRun {run_id} — instance thread dead before dispatch");
        if let Some(complete) = job.complete.take() {
            complete(crate::warm::dead_instance_outcome());
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
    fn handshake(payload: Vec<u8>) -> Option<ipc::RustToTsFrame> {
        let (mut host, runtime) = UnixStream::pair().unwrap();
        let shared = Arc::new(SharedState::new(0, 0));
        let server = std::thread::spawn(move || handle_client(runtime, shared));

        ipc::write_ts_to_rust_frame(&mut host, ipc::TsToRustMessageType::Authenticate, &payload)
            .unwrap();
        host.flush().unwrap();
        let frame = ipc::read_rust_to_ts_frame(&mut host).ok();
        drop(host);
        server.join().unwrap();
        frame
    }

    /// Spawn `handle_client` over a socket pair. Returns the host end, which
    /// the caller keeps open, plus a receiver that fires when the session
    /// thread finishes — so a session that never ends fails the test instead of
    /// hanging the suite.
    fn spawn_session() -> (UnixStream, crossbeam_channel::Receiver<()>) {
        let (host, runtime) = UnixStream::pair().unwrap();
        let shared = Arc::new(SharedState::new(0, 0));
        let (done_tx, done_rx) = crossbeam_channel::bounded(1);
        std::thread::spawn(move || {
            handle_client(runtime, shared);
            let _ = done_tx.send(());
        });
        (host, done_rx)
    }

    #[test]
    fn a_connection_that_never_authenticates_is_dropped() {
        // `_host` stays open for the whole test, so nothing except the deadline
        // can end this session: no EOF, no error, just a peer that says
        // nothing. Before the deadline existed the read blocked forever and the
        // thread was never reclaimed.
        let (_host, done) = spawn_session();

        done.recv_timeout(HANDSHAKE_DEADLINE * 20)
            .expect("a silent peer kept its session thread");
    }

    #[test]
    fn a_first_frame_over_the_handshake_ceiling_is_refused() {
        // Valid probe bytes up front, right protocol version — the frame is
        // refused purely for its size, which is what the pre-handshake ceiling
        // is for: how much an unproven peer can make the runtime buffer. A
        // real probe is a few bytes, so nothing legitimate comes near 4 KiB.
        let mut probe = crate::blob::probe().to_vec();
        probe.resize(5_000, 0);
        let (mut host, done) = spawn_session();

        ipc::write_ts_to_rust_frame(
            &mut host,
            ipc::TsToRustMessageType::Authenticate,
            &authenticate(probe),
        )
        .unwrap();
        host.flush().unwrap();

        done.recv_timeout(HANDSHAKE_DEADLINE * 20)
            .expect("an oversized first frame was not refused");
        assert!(
            ipc::read_rust_to_ts_frame(&mut host).is_err(),
            "an oversized first frame must not be answered"
        );
    }

    fn authenticate(probe: Vec<u8>) -> Vec<u8> {
        ipc::encode_authenticate_payload(&ipc::AuthenticatePayload {
            protocol_version: ipc::PROTOCOL_VERSION,
            probe,
            descriptor_token: vec![0xab; crate::webcodec::DESCRIPTOR_TOKEN_LEN],
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
        let frame =
            handshake(authenticate(crate::blob::probe().to_vec())).expect("expected a Hello frame");
        assert_eq!(frame.message_type, ipc::RustToTsMessageType::Hello);
        let (status, probe, message) = parse_hello(&frame.payload);
        assert_eq!(status, ipc::HelloStatus::Ok as u8);
        assert_eq!(probe, crate::blob::probe());
        assert_eq!(message, "");
    }

    /// The read side of the read/write split: a host whose V8 natively writes
    /// format 16 (Node 27+) is accepted, because this binary READS up to 16
    /// even though it relabels its own writes to 15.
    #[test]
    fn a_format_16_probe_is_accepted() {
        let mut probe = crate::blob::probe().to_vec();
        probe[1] = 0x10;
        let frame = handshake(authenticate(probe)).expect("expected a Hello frame");
        let (status, _, message) = parse_hello(&frame.payload);
        assert_eq!(status, ipc::HelloStatus::Ok as u8);
        assert_eq!(message, "");
    }

    /// The write side of the split: the Hello probe — like every blob this
    /// binary emits — advertises the relabelled format 15, so Node 22–26 can
    /// read it.
    #[test]
    fn the_hello_probe_advertises_write_format_15() {
        let frame =
            handshake(authenticate(crate::blob::probe().to_vec())).expect("expected a Hello frame");
        let (status, probe, _) = parse_hello(&frame.payload);
        assert_eq!(status, ipc::HelloStatus::Ok as u8);
        assert_eq!(probe[0], crate::blob::V8_BLOB_HEADER_TAG);
        assert_eq!(probe[1], 0x0F);
    }

    #[test]
    fn impossible_v8_format_version_is_refused_loudly() {
        // A probe claiming a serialization format far newer than anything this
        // V8 can read. The old protocol closed the socket silently here.
        let probe = vec![crate::blob::V8_BLOB_HEADER_TAG, 0x63, 0x30];
        let frame = handshake(authenticate(probe)).expect("expected a Hello frame");
        let (status, _, message) = parse_hello(&frame.payload);
        assert_eq!(status, ipc::HelloStatus::V8FormatMismatch as u8);
        assert!(
            message.contains("V8 serialization format mismatch"),
            "unhelpful message: {message}"
        );
    }

    #[test]
    fn probe_that_is_not_a_blob_is_refused_loudly() {
        let frame = handshake(authenticate(vec![0x01, 0x02])).expect("expected a Hello frame");
        let (status, _, message) = parse_hello(&frame.payload);
        assert_eq!(status, ipc::HelloStatus::V8FormatMismatch as u8);
        assert!(message.contains("unrecognised format"), "{message}");
    }

    #[test]
    fn protocol_version_mismatch_is_refused_loudly() {
        let payload = ipc::encode_authenticate_payload(&ipc::AuthenticatePayload {
            protocol_version: ipc::PROTOCOL_VERSION + 1,
            probe: crate::blob::probe().to_vec(),
            descriptor_token: vec![0xab; crate::webcodec::DESCRIPTOR_TOKEN_LEN],
        });
        let frame = handshake(payload).expect("expected a Hello frame");
        let (status, _, message) = parse_hello(&frame.payload);
        assert_eq!(status, ipc::HelloStatus::ProtocolVersionMismatch as u8);
        assert!(message.contains("protocol version mismatch"), "{message}");
    }

    // ── The demux: several runs on one connection ─────────────────────────

    use crate::testval::{self, TestValue};

    fn pstr(out: &mut Vec<u8>, s: &str) {
        out.extend_from_slice(&(s.len() as u32).to_be_bytes());
        out.extend_from_slice(s.as_bytes());
    }

    /// A minimal `Run` payload: one bridge global "tool", explicit wall and
    /// (optionally) CPU and memory, everything else at the runtime defaults.
    fn run_payload_full(
        run_id: u32,
        code: &str,
        memory_mb: Option<u32>,
        cpu_ms: Option<u32>,
        wall_ms: u32,
    ) -> Vec<u8> {
        let mut p = Vec::new();
        p.extend_from_slice(&run_id.to_be_bytes());
        pstr(&mut p, code);
        p.push(0); // filename absent
        // limits: memory, cpu, wall, export, stdout, stderr, bridgeBytes,
        // bridgeCalls, grace — each an Optional<u32>.
        match memory_mb {
            Some(v) => {
                p.push(1);
                p.extend_from_slice(&v.to_be_bytes());
            }
            None => p.push(0),
        }
        match cpu_ms {
            Some(v) => {
                p.push(1);
                p.extend_from_slice(&v.to_be_bytes());
            }
            None => p.push(0),
        }
        p.push(1);
        p.extend_from_slice(&wall_ms.to_be_bytes());
        for _ in 0..6 {
            p.push(0);
        }
        // globals: one bridge def
        p.extend_from_slice(&1u32.to_be_bytes());
        p.push(0); // kind = bridge
        pstr(&mut p, "tool");
        p.push(1); // enumerable
        // imports: none
        p.extend_from_slice(&0u32.to_be_bytes());
        p.push(0); // call absent
        p
    }

    fn run_payload_with_cpu(run_id: u32, code: &str, cpu_ms: Option<u32>, wall_ms: u32) -> Vec<u8> {
        run_payload_full(run_id, code, None, cpu_ms, wall_ms)
    }

    fn run_payload(run_id: u32, code: &str, wall_ms: u32) -> Vec<u8> {
        run_payload_full(run_id, code, None, None, wall_ms)
    }

    /// A successful BridgeResponse payload: runId, callId, ok, number value.
    fn bridge_response(run_id: u32, call_id: u32, value: f64) -> Vec<u8> {
        let mut p = Vec::new();
        p.extend_from_slice(&run_id.to_be_bytes());
        p.extend_from_slice(&call_id.to_be_bytes());
        p.push(1); // ok
        p.push(1); // value present
        let blob = testval::to_blob(&TestValue::Number(value));
        p.extend_from_slice(&(blob.len() as u32).to_be_bytes());
        p.extend_from_slice(&blob);
        p
    }

    /// Read frames until a `Result` arrives (skipping none — only
    /// BridgeCall/Result flow here); returns (runId, decoded default export)
    /// for a success payload.
    fn read_success_result(host: &mut UnixStream) -> (u32, TestValue) {
        let frame = ipc::read_rust_to_ts_frame(host).expect("a Result frame");
        assert_eq!(frame.message_type, ipc::RustToTsMessageType::Result);
        let p = &frame.payload;
        let run_id = u32::from_be_bytes(p[0..4].try_into().unwrap());
        assert_eq!(p[4], 1, "run {run_id} must succeed, payload says ok = {}", p[4]);
        assert_eq!(p[5], 1);
        let blob_len = u32::from_be_bytes(p[6..10].try_into().unwrap()) as usize;
        let exports = testval::from_blob(&p[10..10 + blob_len]);
        (run_id, exports)
    }

    /// Read one BridgeCall frame: (runId, callId).
    fn read_bridge_call(host: &mut UnixStream) -> (u32, u32) {
        let frame = ipc::read_rust_to_ts_frame(host).expect("a BridgeCall frame");
        assert_eq!(frame.message_type, ipc::RustToTsMessageType::BridgeCall);
        (
            u32::from_be_bytes(frame.payload[0..4].try_into().unwrap()),
            u32::from_be_bytes(frame.payload[4..8].try_into().unwrap()),
        )
    }

    /// Handshake on an open connection; returns after the Hello.
    fn open_session() -> (UnixStream, crossbeam_channel::Receiver<()>) {
        let (mut host, done) = spawn_session();
        ipc::write_ts_to_rust_frame(
            &mut host,
            ipc::TsToRustMessageType::Authenticate,
            &authenticate(crate::blob::probe().to_vec()),
        )
        .unwrap();
        let hello = ipc::read_rust_to_ts_frame(&mut host).expect("a Hello frame");
        assert_eq!(hello.message_type, ipc::RustToTsMessageType::Hello);
        (host, done)
    }

    #[test]
    fn two_runs_interleave_on_one_connection_with_correct_routing() {
        sandbox::init_platform();
        let (mut host, _done) = open_session();

        // Two runs, in flight simultaneously on ONE connection — the #124
        // acceptance shape. Each suspends on a bridge call.
        let code = "export default await tool()";
        ipc::write_ts_to_rust_frame(
            &mut host,
            ipc::TsToRustMessageType::Run,
            &run_payload(1, code, 10_000),
        )
        .unwrap();
        ipc::write_ts_to_rust_frame(
            &mut host,
            ipc::TsToRustMessageType::Run,
            &run_payload(2, code, 10_000),
        )
        .unwrap();

        // Both BridgeCall frames arrive (worker scheduling decides the
        // order); each leads with its owning run id.
        let mut calls = std::collections::HashMap::from([
            read_bridge_call(&mut host),
            read_bridge_call(&mut host),
        ]);
        assert_eq!(calls.len(), 2, "both runs sent bridge calls");
        let c1 = calls.remove(&1).expect("run 1 sent a call");
        let c2 = calls.remove(&2).expect("run 2 sent a call");

        // Answer run 2 first: the demux must route each response to its own
        // run, and run 2 must complete while run 1 stays suspended.
        ipc::write_ts_to_rust_frame(
            &mut host,
            ipc::TsToRustMessageType::BridgeResponse,
            &bridge_response(2, c2, 22.0),
        )
        .unwrap();
        ipc::write_ts_to_rust_frame(
            &mut host,
            ipc::TsToRustMessageType::BridgeResponse,
            &bridge_response(1, c1, 11.0),
        )
        .unwrap();

        let mut results = std::collections::HashMap::from([
            read_success_result(&mut host),
            read_success_result(&mut host),
        ]);
        let exports_of = |v: TestValue| match v {
            TestValue::Object(fields) => fields
                .into_iter()
                .find(|(k, _)| k == "default")
                .map(|(_, v)| v)
                .expect("a default export"),
            other => panic!("exports blob did not hold an object: {other:?}"),
        };
        assert_eq!(
            exports_of(results.remove(&1).expect("run 1 result")),
            TestValue::Number(11.0)
        );
        assert_eq!(
            exports_of(results.remove(&2).expect("run 2 result")),
            TestValue::Number(22.0)
        );
    }

    #[test]
    fn a_frame_over_a_runs_allowance_fails_that_run_alone() {
        // Item (4)'s narrowed rule: only a frame the host could never have
        // sent (over the flat protocol ceiling) kills the connection. A
        // frame within the ceiling but over ONE run's own inbound allowance
        // (its memoryMb budget) fails that run cleanly — the connection and
        // its other runs keep going.
        sandbox::init_platform();
        let (mut host, _done) = open_session();

        let code = "export default await tool()";
        // Run 1: 8 MB memory budget → 8 MiB inbound allowance.
        ipc::write_ts_to_rust_frame(
            &mut host,
            ipc::TsToRustMessageType::Run,
            &run_payload_full(1, code, Some(8), None, 10_000),
        )
        .unwrap();
        // Run 2: no memory budget (protocol-default allowance).
        ipc::write_ts_to_rust_frame(
            &mut host,
            ipc::TsToRustMessageType::Run,
            &run_payload(2, code, 10_000),
        )
        .unwrap();
        let mut calls = std::collections::HashMap::from([
            read_bridge_call(&mut host),
            read_bridge_call(&mut host),
        ]);
        let c1 = calls.remove(&1).expect("run 1 sent a call");
        let c2 = calls.remove(&2).expect("run 2 sent a call");

        // A 9 MiB response for run 1: inside the wire ceiling, over run 1's
        // allowance.
        let mut oversized = bridge_response(1, c1, 1.0);
        oversized.resize(9 * 1024 * 1024, 0);
        ipc::write_ts_to_rust_frame(
            &mut host,
            ipc::TsToRustMessageType::BridgeResponse,
            &oversized,
        )
        .unwrap();

        // Run 1 fails; the connection survives and run 2 completes.
        let failure = ipc::read_rust_to_ts_frame(&mut host).expect("run 1's failure Result");
        assert_eq!(failure.message_type, ipc::RustToTsMessageType::Result);
        assert_eq!(
            u32::from_be_bytes(failure.payload[0..4].try_into().unwrap()),
            1
        );
        assert_eq!(failure.payload[4], 0, "run 1 must fail on its allowance");

        ipc::write_ts_to_rust_frame(
            &mut host,
            ipc::TsToRustMessageType::BridgeResponse,
            &bridge_response(2, c2, 22.0),
        )
        .unwrap();
        let (run_id, _exports) = read_success_result(&mut host);
        assert_eq!(run_id, 2, "the co-resident run completes normally");
    }

    #[test]
    fn terminate_kills_a_cpu_bound_oneoff_mid_turn() {
        // A synchronous spin with `cpuTimeMs: 0` can only be stopped by the
        // demux's mid-turn kill (`GuardCtl::abort_executing`) — the routed
        // Terminate frame would sit unread forever, and the wall here is 10 s.
        // The prompt ERR_ABORTED (not ERR_WALL_TIMEOUT, not a hang) proves
        // the route's token reaches the one-off's guard.
        sandbox::init_platform();
        let (mut host, _done) = open_session();
        ipc::write_ts_to_rust_frame(
            &mut host,
            ipc::TsToRustMessageType::Run,
            &run_payload_with_cpu(1, "for (;;) {}", Some(0), 10_000),
        )
        .unwrap();
        // Give the worker time to boot its isolate and enter the spin.
        std::thread::sleep(Duration::from_millis(300));
        ipc::write_ts_to_rust_frame(
            &mut host,
            ipc::TsToRustMessageType::Terminate,
            &1u32.to_be_bytes(),
        )
        .unwrap();

        let started = std::time::Instant::now();
        let frame = ipc::read_rust_to_ts_frame(&mut host).expect("a Result frame");
        assert_eq!(frame.message_type, ipc::RustToTsMessageType::Result);
        let p = &frame.payload;
        assert_eq!(u32::from_be_bytes(p[0..4].try_into().unwrap()), 1);
        assert_eq!(p[4], 0, "the aborted run must fail");
        let text = String::from_utf8_lossy(p);
        assert!(text.contains("ERR_ABORTED"), "unexpected failure shape: {text}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the abort must land mid-turn, not at the wall"
        );
    }

    #[test]
    fn a_duplicate_run_id_is_refused_and_the_first_run_survives() {
        sandbox::init_platform();
        let (mut host, _done) = open_session();

        // Run 1 suspends on its bridge call…
        ipc::write_ts_to_rust_frame(
            &mut host,
            ipc::TsToRustMessageType::Run,
            &run_payload(1, "export default await tool()", 10_000),
        )
        .unwrap();
        let (_, c1) = read_bridge_call(&mut host);

        // …then a second Run arrives with the SAME id. Silently replacing
        // the route would strand run 1 (its frames discarded as late), so
        // the duplicate is refused loudly instead.
        ipc::write_ts_to_rust_frame(
            &mut host,
            ipc::TsToRustMessageType::Run,
            &run_payload(1, "export default 2", 10_000),
        )
        .unwrap();
        let refusal = ipc::read_rust_to_ts_frame(&mut host).expect("a refusal Result");
        assert_eq!(refusal.message_type, ipc::RustToTsMessageType::Result);
        assert_eq!(u32::from_be_bytes(refusal.payload[0..4].try_into().unwrap()), 1);
        assert_eq!(refusal.payload[4], 0, "the duplicate must fail");
        let text = String::from_utf8_lossy(&refusal.payload);
        assert!(
            text.contains("already in flight"),
            "unexpected refusal shape: {text}"
        );

        // Run 1 is unharmed: its response still routes and it completes.
        ipc::write_ts_to_rust_frame(
            &mut host,
            ipc::TsToRustMessageType::BridgeResponse,
            &bridge_response(1, c1, 11.0),
        )
        .unwrap();
        let (run_id, exports) = read_success_result(&mut host);
        assert_eq!(run_id, 1);
        match exports {
            TestValue::Object(fields) => {
                let default = fields
                    .into_iter()
                    .find(|(k, _)| k == "default")
                    .map(|(_, v)| v);
                assert_eq!(default, Some(TestValue::Number(11.0)));
            }
            other => panic!("exports blob did not hold an object: {other:?}"),
        }
    }

    #[test]
    fn a_corrupt_frame_fails_the_connections_runs_loudly() {
        sandbox::init_platform();
        let (mut host, done) = open_session();

        // One run in flight, suspended on its bridge call…
        ipc::write_ts_to_rust_frame(
            &mut host,
            ipc::TsToRustMessageType::Run,
            &run_payload(1, "export default await tool()", 10_000),
        )
        .unwrap();
        let _ = read_bridge_call(&mut host);

        // …then framing dies: an unknown frame type. The demux must stop
        // reading (framing is untrustworthy) and every run on THIS
        // connection must fail loudly — never hang, never a wrong-run
        // delivery.
        use std::io::Write;
        host.write_all(&1u32.to_be_bytes()).unwrap();
        host.write_all(&[0xEE]).unwrap();
        host.flush().unwrap();

        let frame = ipc::read_rust_to_ts_frame(&mut host).expect("a failure Result frame");
        assert_eq!(frame.message_type, ipc::RustToTsMessageType::Result);
        let p = &frame.payload;
        assert_eq!(u32::from_be_bytes(p[0..4].try_into().unwrap()), 1);
        assert_eq!(p[4], 0, "the displaced run must fail");
        let text = String::from_utf8_lossy(p);
        assert!(
            text.contains("ERR_INTERNAL") && text.contains("connection closed"),
            "unexpected failure shape: {text}"
        );

        // The demux thread exits after teardown.
        done.recv_timeout(std::time::Duration::from_secs(10))
            .expect("the session thread ends after a corrupt frame");
    }

    #[test]
    fn a_malformed_descriptor_token_is_refused_without_a_reply() {
        // A wrong-size token is a malformed payload: the peer is not speaking
        // this protocol, so the connection closes with no Hello — same policy
        // as any other malformed Authenticate.
        let payload = ipc::encode_authenticate_payload(&ipc::AuthenticatePayload {
            protocol_version: ipc::PROTOCOL_VERSION,
            probe: crate::blob::probe().to_vec(),
            descriptor_token: vec![0xab; 8],
        });
        assert!(
            handshake(payload).is_none(),
            "a malformed token must not be answered"
        );
    }

}
