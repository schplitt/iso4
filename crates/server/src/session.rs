//! Per-connection frame glue. `handle_client` runs the handshake, then
//! becomes the connection's demux: the only socket reader, it hands run
//! requests to the [`Engine`] and routes run-tagged frames to each run's
//! [`RunHandle`] by the leading run id. Runs answer through [`ConnLink`],
//! one serialized writer, so frames from concurrent runs never interleave.
//!
//! A corrupt frame ends the demux: every run in flight on this connection
//! fails cleanly, other connections are untouched.

use std::collections::HashMap;
use std::io::Read;
use std::os::unix::net::UnixStream;
use std::sync::atomic::AtomicU32;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use iso4_core::engine::{Caller, Engine, Refusal, RunHandle};
use iso4_core::link::{Addr, HostLink, Inbound, LinkError, NoopObserver, Outbound};
use iso4_core::trace;
use iso4_types as types;

use crate::ipc;
use crate::wire;

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
        &ipc::encode_hello_payload(status, iso4_core::blob::probe(), message),
    ) {
        eprintln!("[iso4-v8] failed to write rejection Hello frame: {e}");
    }
}

/// The connection's writer as the engine sees it: one frame per message,
/// the run's wire id from the address. A frame over the protocol ceiling is
/// refused synchronously as `TooLarge`, so the sender can substitute.
struct ConnLink {
    outbox: ipc::Outbox,
    runs: ConnRuns,
}

impl HostLink for ConnLink {
    fn send(&self, to: Addr, msg: Outbound) -> Result<(), LinkError> {
        let Addr::Run(run_id) = to;
        let (message_type, head, tail) = match msg {
            Outbound::BridgeCall {
                call_id,
                target,
                args,
            } => {
                let header = wire::encode_bridge_call_header(
                    run_id,
                    call_id,
                    &target,
                    args.len() as u32,
                );
                (ipc::RustToTsMessageType::BridgeCall, header, args)
            }
            Outbound::StreamPull { stream_id, credit } => (
                ipc::RustToTsMessageType::StreamPull,
                ipc::encode_stream_pull_payload(run_id, stream_id, credit),
                Vec::new(),
            ),
            Outbound::StreamCancel { stream_id, reason } => (
                ipc::RustToTsMessageType::StreamCancel,
                ipc::encode_stream_cancel_payload(run_id, stream_id, &reason),
                Vec::new(),
            ),
            Outbound::StreamChunk { stream_id, data } => {
                // Header as head, the chunk as tail: the data is not copied
                // again here.
                let mut head = Vec::with_capacity(12);
                head.extend_from_slice(&run_id.to_be_bytes());
                head.extend_from_slice(&stream_id.to_be_bytes());
                head.extend_from_slice(&(data.len() as u32).to_be_bytes());
                (ipc::RustToTsMessageType::StreamChunk, head, data)
            }
            Outbound::StreamEnd { stream_id, error } => (
                ipc::RustToTsMessageType::StreamEnd,
                ipc::encode_stream_end_payload(run_id, stream_id, error.as_deref()),
                Vec::new(),
            ),
            Outbound::Result { completion } => (
                ipc::RustToTsMessageType::Result,
                wire::encode_run_completion_payload(run_id, completion),
                Vec::new(),
            ),
            Outbound::RunComplete { report } => (
                ipc::RustToTsMessageType::RunComplete,
                wire::encode_run_complete_payload(run_id, &report),
                Vec::new(),
            ),
        };
        self.outbox.send_frame(message_type, head, tail).map_err(|e| {
            if e.kind() == std::io::ErrorKind::InvalidInput {
                LinkError::TooLarge(e.to_string())
            } else {
                LinkError::Closed(e.to_string())
            }
        })
    }

    /// The run's route goes before its final answer, so the host can reuse
    /// the id as soon as it has the answer.
    fn finished(&self, to: Addr) {
        let Addr::Run(run_id) = to;
        self.runs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&run_id);
    }
}

/// Decode a run-tagged frame into the engine's message. The leading run id
/// is routing state the caller already consumed; a malformed payload is
/// reported as the run's own connection loss (the frame cannot be
/// delivered, the connection stays usable for its other runs).
fn inbound_of(frame: ipc::TsToRustFrame) -> Result<Inbound, String> {
    match frame.message_type {
        ipc::TsToRustMessageType::BridgeResponse => {
            wire::parse_bridge_response_payload(&frame.payload)
                .map(|(_run_id, call_id, result)| Inbound::BridgeResponse { call_id, result })
                .map_err(|e| format!("response decode: {e}"))
        }
        ipc::TsToRustMessageType::StreamChunk => {
            let mut payload = frame.payload;
            let chunk = ipc::parse_stream_chunk_payload(&payload)
                .map_err(|e| format!("stream chunk decode: {e}"))?;
            let stream_id = chunk.stream_id;
            // The data is the payload's tail: shift it down in place rather
            // than copying it out.
            let header = payload.len() - chunk.data.len();
            payload.drain(..header);
            Ok(Inbound::StreamChunk {
                stream_id,
                data: payload,
            })
        }
        ipc::TsToRustMessageType::StreamEnd => ipc::parse_stream_end_payload(&frame.payload)
            .map(|end| Inbound::StreamEnd {
                stream_id: end.stream_id,
                error: (!end.ok).then(|| {
                    end.error
                        .unwrap_or_else(|| "the host body source failed".to_string())
                }),
            })
            .map_err(|e| format!("stream end decode: {e}")),
        ipc::TsToRustMessageType::StreamPull => ipc::parse_stream_pull_payload(&frame.payload)
            .map(|pull| Inbound::StreamPull {
                stream_id: pull.stream_id,
                credit: pull.credit,
            })
            .map_err(|e| format!("stream pull decode: {e}")),
        ipc::TsToRustMessageType::StreamCancel => {
            ipc::parse_stream_cancel_payload(&frame.payload)
                .map(|cancel| Inbound::StreamCancel {
                    stream_id: cancel.stream_id,
                    reason: cancel.reason,
                })
                .map_err(|e| format!("stream cancel decode: {e}"))
        }
        other => Err(format!("{other:?} is not a run message")),
    }
}

/// One in-flight run as the demux routes it.
struct RunRoute {
    handle: RunHandle,
    /// Per-run inbound frame cap (the run's memory budget).
    frame_cap: u32,
}

/// This connection's in-flight runs by wire run id, shared between the demux
/// and the link, which drops a run when it finishes.
type ConnRuns = Arc<Mutex<HashMap<u32, RunRoute>>>;

/// The per-run inbound frame allowance: a frame over it fails that run
/// alone. The demux read itself is bounded by the flat protocol ceiling.
fn frame_cap_for(limits: &types::ResourceLimits) -> u32 {
    match (limits.soft_memory_mb, limits.hard_memory_mb) {
        (0, 0) => ipc::DEFAULT_MAX_FRAME_LENGTH,
        (0, hard) => hard.saturating_mul(1024 * 1024),
        (soft, _) => soft.saturating_mul(1024 * 1024),
    }
}

pub fn handle_client(mut stream: UnixStream, engine: Arc<Engine>) {
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
    let host_version = iso4_core::blob::probe_format_version(&auth.probe);
    let read_version = iso4_core::blob::read_format_version();
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
        &ipc::encode_hello_payload(ipc::HelloStatus::Ok, iso4_core::blob::probe(), ""),
    ) {
        eprintln!("[iso4-v8] failed to write Hello frame: {e}");
        return;
    }

    eprintln!("[iso4-v8] handshake complete");

    // ── Step 3: the demux loop ────────────────────────────────────────────
    //
    // This thread is the connection's only reader. Run-tagged frames
    // (BridgeResponse, stream frames, Terminate) route to the owning run's
    // channel; runs execute on worker/instance threads and write their
    // outbound frames through ONE serialized writer, so frames from
    // concurrent runs never interleave mid-frame. Control frames
    // (Precompile, DisposePrefix, Stats) are handled inline here.
    let outbox = match ipc::Outbox::spawn(&stream) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("[iso4-v8] failed to start the connection writer: {e} — closing");
            return;
        }
    };
    let conn_runs: ConnRuns = Arc::new(Mutex::new(HashMap::new()));
    let caller = Caller {
        link: Arc::new(ConnLink {
            outbox: outbox.clone(),
            runs: Arc::clone(&conn_runs),
        }),
        observer: Arc::new(NoopObserver),
        // Bridge call ids never repeat on this connection, across runs.
        call_ids: Arc::new(AtomicU32::new(0)),
        // Only host-type descriptors stamped with this key rehydrate
        // (docs/protocol.md §4.4.6).
        brand_key: iso4_core::webcodec::brand_key_for_token(&auth.descriptor_token),
    };

    // However the demux ends, a panic included, every run in flight on this
    // connection fails cleanly; a warm instance's channel outlives it.
    struct FailRunsOnDrop(ConnRuns);
    impl Drop for FailRunsOnDrop {
        fn drop(&mut self) {
            let routes: Vec<RunRoute> = {
                let mut runs = self.0.lock().unwrap_or_else(|p| p.into_inner());
                runs.drain().map(|(_, r)| r).collect()
            };
            for route in routes {
                route
                    .handle
                    .deliver(Inbound::ConnectionLost { detail: None });
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
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                eprintln!("[iso4-v8] connection closed without close frame");
                break;
            }
            Err(e) => {
                eprintln!("[iso4-v8] connection error: {e}");
                break;
            }
        };

        match frame.message_type {
            ipc::TsToRustMessageType::Close => {
                trace!("[iso4-v8] connection closed by host");
                break;
            }
            ipc::TsToRustMessageType::Authenticate => {
                eprintln!("[iso4-v8] unexpected second Authenticate — closing");
                break;
            }
            ipc::TsToRustMessageType::Run => {
                let req = match ipc::parse_run_payload(&frame.payload) {
                    Ok(p) => p,
                    Err(e) => {
                        eprintln!("[iso4-v8] malformed Run payload: {e} — closing");
                        break;
                    }
                };
                let (run_id, frame_cap) = (req.run_id, frame_cap_for(&req.limits));
                start_run(&conn_runs, &caller, run_id, frame_cap, |register| {
                    engine.run(req, &caller, register)
                });
            }
            ipc::TsToRustMessageType::Precompile => {
                let payload = match ipc::parse_precompile_payload(&frame.payload) {
                    Ok(p) => p,
                    Err(e) => {
                        eprintln!("[iso4-v8] malformed Precompile payload: {e} — closing");
                        break;
                    }
                };
                let prefix_id = engine.prepare(
                    payload.code,
                    payload.filename,
                    payload.globals,
                    payload.imports,
                );
                let result = wire::encode_precompile_result_payload(
                    payload.request_id,
                    Some(&prefix_id),
                    None,
                );
                if let Err(e) = outbox.send_frame(
                    ipc::RustToTsMessageType::PrecompileResult,
                    result,
                    Vec::new(),
                ) {
                    eprintln!("[iso4-v8] failed to write PrecompileResult frame: {e}");
                }
            }
            ipc::TsToRustMessageType::PrefixRun => {
                let req = match ipc::parse_prefix_run_payload(&frame.payload) {
                    Ok(p) => p,
                    Err(e) => {
                        eprintln!("[iso4-v8] malformed PrefixRun payload: {e} — closing");
                        break;
                    }
                };
                let (run_id, frame_cap) = (req.run_id, frame_cap_for(&req.limits));
                start_run(&conn_runs, &caller, run_id, frame_cap, |register| {
                    engine.prefix_run(req, &caller, register)
                });
            }
            ipc::TsToRustMessageType::DisposePrefix => {
                match ipc::parse_dispose_prefix_payload(&frame.payload) {
                    // No response frame: DisposePrefix is fire-and-forget.
                    Ok(prefix_id) => engine.dispose_prefix(&prefix_id),
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
                let stats = engine.stats();
                if let Err(e) = outbox.send_frame(
                    ipc::RustToTsMessageType::StatsResult,
                    wire::encode_stats_payload(&stats),
                    Vec::new(),
                ) {
                    eprintln!("[iso4-v8] failed to write StatsResult frame: {e}");
                    break;
                }
            }
            ipc::TsToRustMessageType::BridgeResponse
            | ipc::TsToRustMessageType::StreamChunk
            | ipc::TsToRustMessageType::StreamEnd
            | ipc::TsToRustMessageType::StreamPull
            | ipc::TsToRustMessageType::StreamCancel => {
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
/// a frame over its run's inbound allowance, or one whose payload does not
/// decode, fails that run alone. A payload too short to carry a run id is
/// CORRUPT — `Err` closes the connection (the blast-radius rule:
/// unparseable framing fails this connection's runs loudly rather than
/// leaving one waiting forever).
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
    let frame_len = frame.payload.len().saturating_add(1);
    let cap = route.frame_cap;
    let handle = route.handle.clone();
    // Decoding copies the payload; finishing runs take this lock.
    drop(runs);
    let msg = if frame_len > cap as usize {
        Inbound::ConnectionLost {
            detail: Some(format!(
                "frame length {frame_len} exceeds max frame length {cap}"
            )),
        }
    } else {
        match inbound_of(frame) {
            Ok(msg) => msg,
            Err(detail) => Inbound::ConnectionLost {
                detail: Some(detail),
            },
        }
    };
    handle.deliver(msg);
    Ok(())
}

/// Route a Terminate to its run; [`RunHandle::deliver`] decides soft or hard.
fn route_terminate(conn_runs: &ConnRuns, frame: ipc::TsToRustFrame) -> Result<(), String> {
    let (run_id, mode) = match ipc::parse_terminate_payload(&frame.payload) {
        Ok(parsed) => parsed,
        // A Terminate that cannot be parsed is corrupt framing — close the
        // connection rather than leaving the (unidentifiable) target running.
        Err(e) => return Err(format!("malformed Terminate payload: {e}")),
    };
    let handle = conn_runs
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(&run_id)
        .map(|route| route.handle.clone());
    let Some(handle) = handle else {
        // The run completed just before the abort landed: a benign race.
        eprintln!("[iso4-v8] ignoring stray Terminate (run {run_id} already completed)");
        return Ok(());
    };
    handle.deliver(Inbound::Terminate { mode });
    Ok(())
}

/// The leading `u32` of a run-tagged payload.
fn peek_run_id(payload: &[u8]) -> Option<u32> {
    payload
        .get(0..4)
        .map(|b| u32::from_be_bytes(b.try_into().expect("4-byte slice")))
}

/// Start a run through the engine, routed under `run_id` once the engine
/// registers it. A duplicate id is refused before anything is acquired:
/// replacing a live route would strand the first run. The demux is this
/// connection's only route inserter, so the check holds through the start.
fn start_run(
    conn_runs: &ConnRuns,
    caller: &Caller,
    run_id: u32,
    frame_cap: u32,
    start: impl FnOnce(&mut dyn FnMut(RunHandle)) -> Result<(), Refusal>,
) {
    let in_flight = conn_runs
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .contains_key(&run_id);
    if in_flight {
        eprintln!(
            "[iso4-v8] run {run_id} is already in flight on this connection — refusing the duplicate"
        );
        let error = iso4_core::v8::RunError::Internal(format!(
            "run id {run_id} is already in flight on this connection; every \
             concurrent run needs a distinct id"
        ));
        refuse(&*caller.link, run_id, error.to_payload());
        return;
    }
    let mut register = |handle: RunHandle| {
        let replaced = conn_runs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(run_id, RunRoute { handle, frame_cap });
        debug_assert!(replaced.is_none(), "run id checked before the start");
    };
    if let Err(refusal) = start(&mut register) {
        conn_runs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&run_id);
        refuse(&*caller.link, run_id, refusal.to_payload());
    }
}

/// Answer a run refused before it started: nothing ran, telemetry is zero.
fn refuse(link: &dyn HostLink, run_id: u32, error: types::RunErrorPayload) {
    let completion = types::RunCompletion::Failure(types::RunFailurePayload::bare(error));
    iso4_core::v8::send_completion(link, run_id, Outbound::Result { completion });
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn run_frame(message_type: ipc::TsToRustMessageType, payload: Vec<u8>) -> ipc::TsToRustFrame {
        ipc::TypedFrame {
            message_type,
            payload,
        }
    }

    #[test]
    fn run_frames_decode_into_engine_messages() {
        let mut chunk = Vec::new();
        chunk.extend_from_slice(&9u32.to_be_bytes());
        chunk.extend_from_slice(&2u32.to_be_bytes());
        chunk.extend_from_slice(&3u32.to_be_bytes());
        chunk.extend_from_slice(b"abc");
        match inbound_of(run_frame(ipc::TsToRustMessageType::StreamChunk, chunk)).unwrap() {
            Inbound::StreamChunk { stream_id, data } => {
                assert_eq!((stream_id, data.as_slice()), (2, &b"abc"[..]));
            }
            other => panic!("expected a chunk, got {other:?}"),
        }

        // ok = false with no message names the host source.
        let mut end = Vec::new();
        end.extend_from_slice(&9u32.to_be_bytes());
        end.extend_from_slice(&2u32.to_be_bytes());
        end.push(0);
        end.push(0);
        match inbound_of(run_frame(ipc::TsToRustMessageType::StreamEnd, end)).unwrap() {
            Inbound::StreamEnd { stream_id, error } => {
                assert_eq!(stream_id, 2);
                assert_eq!(error.as_deref(), Some("the host body source failed"));
            }
            other => panic!("expected an end, got {other:?}"),
        }

        let short = vec![0u8; 6];
        let err = inbound_of(run_frame(ipc::TsToRustMessageType::StreamChunk, short)).unwrap_err();
        assert!(err.contains("stream chunk decode"), "{err}");
    }

    /// Drive `handle_client` over a socket pair with the given Authenticate
    /// payload and return the first frame it writes back (if any).
    fn handshake(payload: Vec<u8>) -> Option<ipc::RustToTsFrame> {
        let (mut host, runtime) = UnixStream::pair().unwrap();
        let engine = Arc::new(Engine::new(0, 0));
        let server = std::thread::spawn(move || handle_client(runtime, engine));

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
        let engine = Arc::new(Engine::new(0, 0));
        let (done_tx, done_rx) = crossbeam_channel::bounded(1);
        std::thread::spawn(move || {
            handle_client(runtime, engine);
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
        let mut probe = iso4_core::blob::probe().to_vec();
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
            descriptor_token: vec![0xab; iso4_core::webcodec::DESCRIPTOR_TOKEN_LEN],
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
            handshake(authenticate(iso4_core::blob::probe().to_vec())).expect("expected a Hello frame");
        assert_eq!(frame.message_type, ipc::RustToTsMessageType::Hello);
        let (status, probe, message) = parse_hello(&frame.payload);
        assert_eq!(status, ipc::HelloStatus::Ok as u8);
        assert_eq!(probe, iso4_core::blob::probe());
        assert_eq!(message, "");
    }

    /// The read side of the read/write split: a host whose V8 natively writes
    /// format 16 (Node 27+) is accepted, because this binary READS up to 16
    /// even though it relabels its own writes to 15.
    #[test]
    fn a_format_16_probe_is_accepted() {
        let mut probe = iso4_core::blob::probe().to_vec();
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
            handshake(authenticate(iso4_core::blob::probe().to_vec())).expect("expected a Hello frame");
        let (status, probe, _) = parse_hello(&frame.payload);
        assert_eq!(status, ipc::HelloStatus::Ok as u8);
        assert_eq!(probe[0], iso4_core::blob::V8_BLOB_HEADER_TAG);
        assert_eq!(probe[1], 0x0F);
    }

    #[test]
    fn impossible_v8_format_version_is_refused_loudly() {
        // A probe claiming a serialization format far newer than anything this
        // V8 can read. The old protocol closed the socket silently here.
        let probe = vec![iso4_core::blob::V8_BLOB_HEADER_TAG, 0x63, 0x30];
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
            probe: iso4_core::blob::probe().to_vec(),
            descriptor_token: vec![0xab; iso4_core::webcodec::DESCRIPTOR_TOKEN_LEN],
        });
        let frame = handshake(payload).expect("expected a Hello frame");
        let (status, _, message) = parse_hello(&frame.payload);
        assert_eq!(status, ipc::HelloStatus::ProtocolVersionMismatch as u8);
        assert!(message.contains("protocol version mismatch"), "{message}");
    }

    // ── The demux: several runs on one connection ─────────────────────────

    use iso4_core::testval::{self, TestValue};

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
        // bridgeCalls, grace, hardMemory — each an Optional<u32>.
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
        for _ in 0..7 {
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
        // p[4..8] is slotAllowance.
        assert_eq!(p[8], 1, "run {run_id} must succeed, payload says ok = {}", p[8]);
        assert_eq!(p[9], 1);
        let blob_len = u32::from_be_bytes(p[10..14].try_into().unwrap()) as usize;
        let exports = testval::from_blob(&p[14..14 + blob_len]);
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
            &authenticate(iso4_core::blob::probe().to_vec()),
        )
        .unwrap();
        let hello = ipc::read_rust_to_ts_frame(&mut host).expect("a Hello frame");
        assert_eq!(hello.message_type, ipc::RustToTsMessageType::Hello);
        (host, done)
    }

    #[test]
    fn a_close_frame_ends_the_session_while_the_socket_stays_open() {
        let (mut host, done) = open_session();

        ipc::write_ts_to_rust_frame(&mut host, ipc::TsToRustMessageType::Close, &[]).unwrap();

        done.recv_timeout(Duration::from_secs(5))
            .expect("a Close frame did not end the session");
        assert!(
            ipc::read_rust_to_ts_frame(&mut host).is_err(),
            "the runtime closes its end after Close"
        );
    }

    #[test]
    fn two_runs_interleave_on_one_connection_with_correct_routing() {
        iso4_core::v8::init_platform();
        let (mut host, _done) = open_session();

        // Two runs in flight at once on one connection, each suspended on a
        // bridge call.
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
        iso4_core::v8::init_platform();
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
        assert_eq!(failure.payload[8], 0, "run 1 must fail on its allowance");

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
    fn hard_terminate_kills_a_cpu_bound_oneoff_mid_turn() {
        // A synchronous spin with `cpuTimeMs: 0` can only be stopped by the
        // HARD abort's mid-turn kill (`GuardCtl::abort_executing`) — a soft
        // (routed) Terminate would sit unread forever, and the wall here is
        // 10 s. The prompt ERR_ABORTED (not ERR_WALL_TIMEOUT, not a hang)
        // proves the route's token reaches the one-off's guard.
        iso4_core::v8::init_platform();
        let (mut host, _done) = open_session();
        ipc::write_ts_to_rust_frame(
            &mut host,
            ipc::TsToRustMessageType::Run,
            &run_payload_with_cpu(1, "for (;;) {}", Some(0), 10_000),
        )
        .unwrap();
        // Give the worker time to boot its isolate and enter the spin.
        std::thread::sleep(Duration::from_millis(300));
        let mut terminate = 1u32.to_be_bytes().to_vec();
        terminate.push(1); // hard
        ipc::write_ts_to_rust_frame(&mut host, ipc::TsToRustMessageType::Terminate, &terminate)
            .unwrap();

        let started = std::time::Instant::now();
        let frame = ipc::read_rust_to_ts_frame(&mut host).expect("a Result frame");
        assert_eq!(frame.message_type, ipc::RustToTsMessageType::Result);
        let p = &frame.payload;
        assert_eq!(u32::from_be_bytes(p[0..4].try_into().unwrap()), 1);
        assert_eq!(p[8], 0, "the aborted run must fail");
        let text = String::from_utf8_lossy(p);
        assert!(text.contains("ERR_ABORTED"), "unexpected failure shape: {text}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the abort must land mid-turn, not at the wall"
        );
    }

    #[test]
    fn hard_terminate_mid_continuation_concludes_promptly() {
        // The kill lands inside a microtask-checkpoint continuation, where
        // V8's terminate flag used to be consumed silently: prompt
        // ERR_ABORTED, never a hang to the wall.
        iso4_core::v8::init_platform();
        let (mut host, _done) = open_session();
        ipc::write_ts_to_rust_frame(
            &mut host,
            ipc::TsToRustMessageType::Run,
            &run_payload_with_cpu(
                1,
                "export default await tool().then(() => { for (;;) {} })",
                Some(0),
                10_000,
            ),
        )
        .unwrap();
        let (run_id, call_id) = read_bridge_call(&mut host);
        ipc::write_ts_to_rust_frame(
            &mut host,
            ipc::TsToRustMessageType::BridgeResponse,
            &bridge_response(run_id, call_id, 1.0),
        )
        .unwrap();
        // Give the response turn time to enter the spinning continuation.
        std::thread::sleep(Duration::from_millis(300));
        let mut terminate = 1u32.to_be_bytes().to_vec();
        terminate.push(1); // hard
        ipc::write_ts_to_rust_frame(&mut host, ipc::TsToRustMessageType::Terminate, &terminate)
            .unwrap();

        let started = std::time::Instant::now();
        let frame = ipc::read_rust_to_ts_frame(&mut host).expect("a Result frame");
        assert_eq!(frame.message_type, ipc::RustToTsMessageType::Result);
        let p = &frame.payload;
        assert_eq!(u32::from_be_bytes(p[0..4].try_into().unwrap()), 1);
        assert_eq!(p[8], 0, "the aborted run must fail");
        let text = String::from_utf8_lossy(p);
        assert!(text.contains("ERR_ABORTED"), "unexpected failure shape: {text}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the hard abort must conclude promptly, not hang to the wall"
        );
    }

    #[test]
    fn soft_terminate_mid_turn_abandons_at_the_boundary() {
        // A soft Terminate arriving while the run's turn is executing is
        // NOT delivered mid-JS: it queues and lands once the turn yields.
        // The continuation here burns briefly and then suspends forever on
        // a fresh promise — the abort must land right after that turn, as
        // a clean abandon, not hang and not interrupt.
        iso4_core::v8::init_platform();
        let (mut host, _done) = open_session();
        ipc::write_ts_to_rust_frame(
            &mut host,
            ipc::TsToRustMessageType::Run,
            &run_payload(
                1,
                "export default await tool().then(() => new Promise(() => {}))",
                10_000,
            ),
        )
        .unwrap();
        let (run_id, call_id) = read_bridge_call(&mut host);
        // Response first, soft Terminate right behind it: the demux routes
        // both in order, so the abort is queued while the response turn
        // runs and abandons the run at its boundary.
        ipc::write_ts_to_rust_frame(
            &mut host,
            ipc::TsToRustMessageType::BridgeResponse,
            &bridge_response(run_id, call_id, 1.0),
        )
        .unwrap();
        let mut terminate = 1u32.to_be_bytes().to_vec();
        terminate.push(0); // soft
        ipc::write_ts_to_rust_frame(&mut host, ipc::TsToRustMessageType::Terminate, &terminate)
            .unwrap();

        let started = std::time::Instant::now();
        let frame = ipc::read_rust_to_ts_frame(&mut host).expect("a Result frame");
        assert_eq!(frame.message_type, ipc::RustToTsMessageType::Result);
        let p = &frame.payload;
        assert_eq!(u32::from_be_bytes(p[0..4].try_into().unwrap()), 1);
        assert_eq!(p[8], 0, "the aborted run must fail");
        let text = String::from_utf8_lossy(p);
        assert!(text.contains("ERR_ABORTED"), "unexpected failure shape: {text}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the soft abort must land at the turn boundary, not the wall"
        );
    }

    #[test]
    fn a_duplicate_run_id_is_refused_and_the_first_run_survives() {
        iso4_core::v8::init_platform();
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
    fn a_finished_runs_id_can_be_used_again() {
        iso4_core::v8::init_platform();
        let (mut host, _done) = open_session();
        for _ in 0..2 {
            ipc::write_ts_to_rust_frame(
                &mut host,
                ipc::TsToRustMessageType::Run,
                &run_payload(1, "export default 7", 10_000),
            )
            .unwrap();
            let (run_id, _exports) = read_success_result(&mut host);
            assert_eq!(run_id, 1);
        }
    }

    #[test]
    fn a_corrupt_frame_fails_the_connections_runs_loudly() {
        iso4_core::v8::init_platform();
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
            probe: iso4_core::blob::probe().to_vec(),
            descriptor_token: vec![0xab; 8],
        });
        assert!(
            handshake(payload).is_none(),
            "a malformed token must not be answered"
        );
    }

}
