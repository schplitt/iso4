//! Streamed bodies in both directions: host-type bodies hydrated into guest
//! streams under the host's credit window, and guest streams pulled and
//! delivered to the host chunk by chunk.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::link::{Addr, HostLink, Inbound, Outbound};
use crate::types;
use crate::v8::RunError;
use crate::v8::exceptions::error_message_from_value;
use crate::v8::globals::{throw_v8_error, v8_error_value};
use crate::v8::runloop::RunState;
use crate::v8::runtable::{CURRENT_RUN_TOKEN, RUN_TABLE_PTR, attributed_state, slot_table};

// ── Streamed host-type bodies ────────────────────────────────────────────────

/// Runtime-side state of one streamed body: chunks the host has sent that
/// the sandbox has not read yet, terminal state, and the sandbox read (at
/// most one) waiting for data.
pub(super) struct StreamState {
    buffered: std::collections::VecDeque<Vec<u8>>,
    buffered_bytes: usize,
    /// `Some(Ok(()))` after a clean `StreamEnd`; `Some(Err(msg))` after a
    /// source failure. Reads observe EOF / rejection once the buffer drains.
    ended: Option<Result<(), String>>,
    cancelled: bool,
    pending: Option<v8::Global<v8::PromiseResolver>>,
}

impl StreamState {
    fn new() -> Self {
        Self {
            buffered: std::collections::VecDeque::new(),
            buffered_bytes: 0,
            ended: None,
            cancelled: false,
            pending: None,
        }
    }
}

/// Per-run registry of streamed bodies. Instance-lifetime Box (stable
/// address, the `PendingWork` pattern); armed per call, disarmed after —
/// hydrating or reading a stream outside a run fails cleanly.
pub struct StreamTable {
    pub(super) slot: RefCell<Option<StreamTableState>>,
}

pub(super) struct StreamTableState {
    pub(super) streams: HashMap<u32, StreamState>,
    /// The run's address on outbound `StreamPull`/`StreamCancel` messages.
    pub(super) run_id: u32,
    pub(super) link: Option<Arc<dyn HostLink>>,
}

impl StreamTable {
    pub(super) fn boxed() -> Box<Self> {
        Box::new(Self {
            slot: RefCell::new(None),
        })
    }
}

// ── Outbound (sandbox → host) streamed bodies ────────────────────────────────

/// How long an outbound stream may sit with no host activity (no credit
/// grant, no cancel) before the runtime drops it (Jakob's ruling: 10 s —
/// our consumer is in-process JS with no socket to error on abandonment).
const OUT_STREAM_IDLE_MS: u64 = 10_000;

/// Test hook: a nonzero override shrinks the idle allowance so the
/// deadline path is testable without 10 s waits.
#[cfg(test)]
pub(crate) static OUT_IDLE_OVERRIDE_MS: AtomicU64 = AtomicU64::new(0);

fn out_stream_idle_ms() -> u64 {
    #[cfg(test)]
    {
        let v = OUT_IDLE_OVERRIDE_MS.load(Ordering::Relaxed);
        if v > 0 {
            return v;
        }
    }
    OUT_STREAM_IDLE_MS
}

/// Runtime-side state of one outbound streamed body: the guest stream
/// driving it, the host-granted credit, and the pull in flight.
pub(super) struct OutStream {
    /// The guest `Iso4BodyStream` this stream pulls from.
    body: v8::Global<v8::Object>,
    /// Bytes the host still allows in flight. Seeded with the implicit
    /// initial window (eager production, mirroring the inbound direction).
    credit: u32,
    /// A started pull whose promise has not settled yet.
    pending: Option<v8::Global<v8::Promise>>,
    /// Unsent remainder of a guest chunk that outgrew the credit/chunk cap.
    backlog: Vec<u8>,
    backlog_sent: usize,
    /// A terminal frame (`StreamEnd`) went out or a cancel arrived; the
    /// entry only remains so late frames are recognized.
    done: bool,
}

/// Per-run registry of outbound streamed bodies — the [`StreamTable`]
/// pattern with the direction reversed. Armed per run; result serialization
/// registers into it through the per-thread run pointers.
pub struct OutStreamTable {
    pub(super) slot: RefCell<Option<OutTableState>>,
}

pub(super) struct OutTableState {
    pub(super) streams: HashMap<u32, OutStream>,
    pub(super) next_id: u32,
    /// The run's address on outbound `StreamChunk`/`StreamEnd` messages.
    pub(super) run_id: u32,
    pub(super) link: Option<Arc<dyn HostLink>>,
    /// Last host read/cancel — the idle-deadline anchor.
    pub(super) last_activity: std::time::Instant,
}

impl OutStreamTable {
    pub(super) fn boxed() -> Box<Self> {
        Box::new(Self {
            slot: RefCell::new(None),
        })
    }
}

/// Register a guest stream as an outbound body during RESULT serialization.
/// Returns the allocated stream id, or `None` when the executing run cannot
/// stream (no run, table not armed — hold-mode runs, or a leg where stream
/// bodies are not allowed; the codec turns `None` into its own error).
pub fn register_out_stream(
    scope: &mut v8::PinScope,
    body: v8::Local<v8::Object>,
) -> Option<u32> {
    if !RESULT_STREAM_MODE.with(|c| c.get()) {
        return None;
    }
    let token = CURRENT_RUN_TOKEN.with(|c| c.get());
    if token == 0 {
        return None;
    }
    RUN_TABLE_PTR.with(|c| {
        let ptr = c.get();
        if ptr.is_null() {
            return None;
        }
        // SAFETY: set only while the owning instance's turn is live on this
        // thread (same contract as `register_hydrated_stream`).
        let table = unsafe { &*ptr };
        let state = table.state_ptr(token);
        if state.is_null() {
            return None;
        }
        // SAFETY: entry pointers are valid for the remainder of the turn.
        let mut slot = unsafe { &*state }.out_streams.slot.borrow_mut();
        let st = slot.as_mut()?;
        let id = st.next_id;
        st.next_id += 1;
        st.streams.insert(
            id,
            OutStream {
                body: v8::Global::new(scope, body),
                credit: types::STREAM_CREDIT_WINDOW_BYTES,
                pending: None,
                backlog: Vec::new(),
                backlog_sent: 0,
                done: false,
            },
        );
        Some(id)
    })
}

thread_local! {
    /// Whether the value currently being serialized is a run RESULT on a
    /// leg that may carry stream bodies (session runs with an epilogue).
    /// Off for exports, bridge arguments, and hold-mode runs — a stream
    /// body there keeps the existing cannot-cross error.
    static RESULT_STREAM_MODE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// RAII enabling [`register_out_stream`] for one serialization.
pub(super) struct ResultStreamMode;
impl ResultStreamMode {
    pub(super) fn enter(enabled: bool) -> Option<Self> {
        if !enabled {
            return None;
        }
        RESULT_STREAM_MODE.with(|c| c.set(true));
        Some(Self)
    }
}
impl Drop for ResultStreamMode {
    fn drop(&mut self) {
        RESULT_STREAM_MODE.with(|c| c.set(false));
    }
}

/// Register a hydrated stream in the executing run's table. Returns false
/// when no run is executing on this thread (a descriptor carrying a stream
/// outside a run — refused by the codec). The codec walk has no `scope`, so
/// attribution is the turn owner: rehydration only happens while delivering
/// that run's own payloads (call args, its bridge responses).
pub fn register_hydrated_stream(stream_id: u32) -> bool {
    let token = CURRENT_RUN_TOKEN.with(|c| c.get());
    if token == 0 {
        return false;
    }
    RUN_TABLE_PTR.with(|c| {
        let ptr = c.get();
        if ptr.is_null() {
            return false;
        }
        // SAFETY: set only while the owning instance's turn is live on this
        // thread.
        let table = unsafe { &*ptr };
        let state = table.state_ptr(token);
        if state.is_null() {
            return false;
        }
        // SAFETY: entry pointers are valid for the remainder of the turn.
        let mut slot = unsafe { &*state }.streams.slot.borrow_mut();
        match slot.as_mut() {
            Some(state) => {
                state.streams.insert(stream_id, StreamState::new());
                true
            }
            None => false,
        }
    })
}

/// Send a `StreamPull` credit grant for consumed bytes. Best-effort: a dead
/// link surfaces through the run's own events soon enough.
fn send_stream_credit(state: &StreamTableState, stream_id: u32, credit: u32) {
    let Some(link) = &state.link else { return };
    if credit == 0 {
        return;
    }
    let _ = link.send(
        Addr::Run(state.run_id),
        Outbound::StreamPull { stream_id, credit },
    );
}

/// Send a `StreamCancel` so the host stops pumping and releases the source.
fn send_stream_cancel(state: &StreamTableState, stream_id: u32, reason: &str) {
    let Some(link) = &state.link else { return };
    let _ = link.send(
        Addr::Run(state.run_id),
        Outbound::StreamCancel {
            stream_id,
            reason: reason.to_string(),
        },
    );
}

/// Whether the run has outbound streams the host may still read.
pub(super) fn out_streams_open(rs: &RunState) -> bool {
    // SAFETY: `out_streams` points into the run's live table entry.
    unsafe { &*rs.out_streams }
        .slot
        .borrow()
        .as_ref()
        .is_some_and(|t| t.streams.values().any(|st| !st.done))
}

/// The idle deadline while outbound streams are open: last host activity
/// plus the 10 s allowance (Jakob's ruling — an in-process consumer has no
/// socket to error on abandonment).
pub(super) fn out_idle_deadline(rs: &RunState) -> Option<std::time::Instant> {
    // SAFETY: `out_streams` points into the run's live table entry.
    let table = unsafe { &*rs.out_streams };
    let slot = table.slot.borrow();
    let st = slot.as_ref()?;
    if st.streams.values().any(|s| !s.done) {
        Some(st.last_activity + Duration::from_millis(out_stream_idle_ms()))
    } else {
        None
    }
}

/// Apply a host credit grant to an outbound stream (unknown or done streams
/// ignore it).
pub(super) fn note_out_credit(rs: &RunState, stream_id: u32, credit: u32) {
    // SAFETY: `out_streams` points into the run's live table entry.
    let table = unsafe { &*rs.out_streams };
    let mut slot = table.slot.borrow_mut();
    let Some(st) = slot.as_mut() else { return };
    st.last_activity = std::time::Instant::now();
    if let Some(stream) = st.streams.get_mut(&stream_id) {
        if !stream.done {
            stream.credit = stream.credit.saturating_add(credit);
        }
    }
}

/// Note host activity on the run's outbound table (a cancel counts — the
/// idle deadline measures abandonment, not consumption).
pub(super) fn note_out_activity(rs: &RunState) {
    // SAFETY: `out_streams` points into the run's live table entry.
    let table = unsafe { &*rs.out_streams };
    let mut slot = table.slot.borrow_mut();
    let Some(st) = slot.as_mut() else { return };
    st.last_activity = std::time::Instant::now();
}

/// Best-effort, turn-less termination of every open outbound stream: one
/// `StreamEnd` error each (dead links ignored), no guest cleanup —
/// the truncate/kill/give-up paths, where no cleanup slice runs.
pub(super) fn close_out_streams(out_streams: *const OutStreamTable, message: &str) {
    // SAFETY: `out_streams` points into the run's live table entry.
    let table = unsafe { &*out_streams };
    let mut slot = table.slot.borrow_mut();
    let Some(st) = slot.as_mut() else { return };
    let run_id = st.run_id;
    let link = st.link.clone();
    for (id, stream) in st.streams.iter_mut() {
        if stream.done {
            continue;
        }
        stream.done = true;
        stream.pending = None;
        stream.backlog = Vec::new();
        if let Some(link) = &link {
            let _ = link.send(
                Addr::Run(run_id),
                Outbound::StreamEnd {
                    stream_id: *id,
                    error: Some(message.to_string()),
                },
            );
        }
    }
}

/// Cancel one outbound stream with GUEST cleanup (web-streams semantics):
/// invokes the stream's internal cancel path — for an async-iterable body
/// that is `iterator.return()`, so `finally` blocks run; for a pass-through
/// it cancels the upstream inbound stream — as part of the current turn,
/// then marks it done. `notify_host` additionally sends a `StreamEnd`
/// error frame (the idle deadline / connection paths; a host-initiated
/// cancel sends nothing back).
pub(super) fn cancel_out_stream(
    scope: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
    rs: &RunState,
    id: u32,
    reason: &str,
    notify_host: bool,
) {
    let body = {
        // SAFETY: `out_streams` points into the run's live table entry.
        let table = unsafe { &*rs.out_streams };
        let mut slot = table.slot.borrow_mut();
        let Some(st) = slot.as_mut() else { return };
        let run_id = st.run_id;
        let link = st.link.clone();
        let Some(stream) = st.streams.get_mut(&id) else { return };
        if stream.done {
            return;
        }
        stream.done = true;
        stream.pending = None;
        stream.backlog = Vec::new();
        if notify_host {
            if let Some(link) = &link {
                let _ = link.send(
                    Addr::Run(run_id),
                    Outbound::StreamEnd {
                        stream_id: id,
                        error: Some(reason.to_string()),
                    },
                );
            }
        }
        stream.body.clone()
    };
    let body_local = v8::Local::new(scope, &body);
    let Some(name) = v8::String::new(scope, "_cancelSource") else { return };
    let Some(method) = body_local.get(scope, name.into()) else { return };
    let Ok(method) = v8::Local::<v8::Function>::try_from(method) else { return };
    let reason_v8: v8::Local<v8::Value> = match v8::String::new(scope, reason) {
        Some(r) => r.into(),
        None => v8::undefined(scope).into(),
    };
    if method.call(scope, body_local.into(), &[reason_v8]).is_none() {
        // Guest cleanup threw (or was killed — the caller's tail sees the
        // reason cell). A plain throw in cleanup is swallowed: the stream
        // is already over either way.
        if !scope.is_execution_terminating() {
            scope.reset();
        }
    }
}

/// Cancel every open outbound stream with guest cleanup — the idle-deadline
/// and connection-death paths.
pub(super) fn cancel_all_out_streams(
    scope: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
    rs: &RunState,
    reason: &str,
) {
    let ids: Vec<u32> = {
        // SAFETY: `out_streams` points into the run's live table entry.
        let table = unsafe { &*rs.out_streams };
        let slot = table.slot.borrow();
        match slot.as_ref() {
            Some(st) => st
                .streams
                .iter()
                .filter(|(_, s)| !s.done)
                .map(|(id, _)| *id)
                .collect(),
            None => Vec::new(),
        }
    };
    for id in ids {
        cancel_out_stream(scope, rs, id, reason, true);
        if scope.is_execution_terminating() {
            break;
        }
    }
}

/// Extract a pull result: `Ok(Some(bytes))` for a chunk, `Ok(None)` at EOF,
/// `Err(message)` for anything a body may not yield.
fn chunk_from_pull(
    scope: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
    value: v8::Local<v8::Value>,
) -> Result<Option<Vec<u8>>, String> {
    if value.is_null_or_undefined() {
        return Ok(None);
    }
    if let Ok(u8s) = v8::Local::<v8::Uint8Array>::try_from(value) {
        let mut bytes = vec![0u8; u8s.byte_length()];
        let copied = u8s.copy_contents(&mut bytes);
        bytes.truncate(copied);
        return Ok(Some(bytes));
    }
    if value.is_string() {
        let Some(text) = value.to_string(scope) else {
            return Err("body stream chunk could not be read".to_string());
        };
        return Ok(Some(text.to_rust_string_lossy(scope).into_bytes()));
    }
    Err("a stream body must yield Uint8Array or string chunks".to_string())
}

/// Start one pull on the guest stream: calls its `_pull()` and returns the
/// promise (a non-promise return is wrapped). `Err` carries the message of
/// a synchronous throw.
fn start_body_pull<'s>(
    scope: &mut v8::PinnedRef<'s, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
    body: v8::Local<'s, v8::Object>,
) -> Result<v8::Local<'s, v8::Promise>, String> {
    let Some(name) = v8::String::new(scope, "_pull") else {
        return Err("could not intern _pull".to_string());
    };
    let Some(method) = body.get(scope, name.into()) else {
        return Err("body stream has no pull".to_string());
    };
    let Ok(method) = v8::Local::<v8::Function>::try_from(method) else {
        return Err("body stream pull is not callable".to_string());
    };
    let Some(result) = method.call(scope, body.into(), &[]) else {
        if scope.is_execution_terminating() {
            return Err("terminated".to_string());
        }
        let message = scope
            .exception()
            .and_then(|e| error_message_from_value(scope, e))
            .unwrap_or_else(|| "body stream pull failed".to_string());
        scope.reset();
        return Err(message);
    };
    if let Ok(promise) = v8::Local::<v8::Promise>::try_from(result) {
        return Ok(promise);
    }
    let Some(resolver) = v8::PromiseResolver::new(scope) else {
        return Err("could not wrap pull result".to_string());
    };
    resolver.resolve(scope, result);
    Ok(resolver.get_promise(scope))
}

/// A drive pass cap: each round may start pulls whose promises settle
/// within one checkpoint (a sync-ish source); credit bounds the real work,
/// this only bounds rounds inside ONE turn.
const OUT_DRIVE_MAX_ROUNDS: usize = 32;

/// Advance every outbound stream as far as host credit and the guest
/// sources allow. Guest pulls execute as part of the CURRENT turn (billed
/// to the phase budgets); a pull suspended on host IO parks until its
/// promise settles in a later turn, when the scan calls back in here.
pub(super) fn drive_out_streams(
    scope: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
    rs: &RunState,
) {
    let ids: Vec<u32> = {
        // SAFETY: `out_streams` points into the run's live table entry.
        let table = unsafe { &*rs.out_streams };
        let slot = table.slot.borrow();
        match slot.as_ref() {
            Some(st) => st.streams.keys().copied().collect(),
            None => return,
        }
    };
    if ids.is_empty() {
        return;
    }
    for _ in 0..OUT_DRIVE_MAX_ROUNDS {
        let mut progressed = false;
        for &id in &ids {
            progressed |= drive_out_stream(scope, rs, id);
            if scope.is_execution_terminating() {
                return;
            }
        }
        if !progressed {
            return;
        }
        // Settle pulls started this round (async sources resolve on the
        // microtask queue when their data is already available).
        scope.perform_microtask_checkpoint();
        if scope.is_execution_terminating() {
            return;
        }
    }
}

/// One stream's drive step. Returns whether it made progress (sent bytes,
/// settled a pull, started one) — the caller checkpoints and loops.
fn drive_out_stream(
    scope: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
    rs: &RunState,
    id: u32,
) -> bool {
    enum Step {
        Idle,
        AwaitPending(v8::Global<v8::Promise>),
        StartPull(v8::Global<v8::Object>),
    }
    /// Flush backlog under credit (no JS) and classify the next step.
    fn classify(rs: &RunState, id: u32, progressed: &mut bool) -> Step {
        // SAFETY: `out_streams` points into the run's live table entry.
        let table = unsafe { &*rs.out_streams };
        let mut slot = table.slot.borrow_mut();
        let Some(st) = slot.as_mut() else { return Step::Idle };
        let run_id = st.run_id;
        let link = st.link.clone();
        let Some(stream) = st.streams.get_mut(&id) else { return Step::Idle };
        if stream.done {
            return Step::Idle;
        }
        while stream.credit > 0 && stream.backlog_sent < stream.backlog.len() {
            let remaining = stream.backlog.len() - stream.backlog_sent;
            let n = remaining
                .min(stream.credit as usize)
                .min(types::STREAM_CHUNK_MAX_BYTES as usize);
            let Some(link) = &link else {
                stream.done = true;
                return Step::Idle;
            };
            let data = stream.backlog[stream.backlog_sent..stream.backlog_sent + n].to_vec();
            if link
                .send(Addr::Run(run_id), Outbound::StreamChunk { stream_id: id, data })
                .is_err()
            {
                // Dead link: the connection-loss event ends the phase.
                stream.done = true;
                return Step::Idle;
            }
            stream.backlog_sent += n;
            stream.credit -= n as u32;
            *progressed = true;
        }
        if stream.backlog_sent >= stream.backlog.len() && !stream.backlog.is_empty() {
            stream.backlog = Vec::new();
            stream.backlog_sent = 0;
        }
        if !stream.backlog.is_empty() || stream.credit == 0 {
            return Step::Idle; // waiting for credit
        }
        if let Some(p) = &stream.pending {
            return Step::AwaitPending(p.clone());
        }
        Step::StartPull(stream.body.clone())
    }
    /// Conclude a settled pull into the stream (no JS).
    fn settle(rs: &RunState, id: u32, outcome: Result<Option<Vec<u8>>, String>) {
        // SAFETY: `out_streams` points into the run's live table entry.
        let table = unsafe { &*rs.out_streams };
        let mut slot = table.slot.borrow_mut();
        let Some(st) = slot.as_mut() else { return };
        let run_id = st.run_id;
        let link = st.link.clone();
        let Some(stream) = st.streams.get_mut(&id) else { return };
        stream.pending = None;
        match outcome {
            Ok(Some(bytes)) => {
                stream.backlog = bytes;
                stream.backlog_sent = 0;
            }
            Ok(None) => {
                stream.done = true;
                if let Some(link) = &link {
                    let _ = link.send(
                        Addr::Run(run_id),
                        Outbound::StreamEnd {
                            stream_id: id,
                            error: None,
                        },
                    );
                }
            }
            Err(message) => {
                stream.done = true;
                if let Some(link) = &link {
                    let _ = link.send(
                        Addr::Run(run_id),
                        Outbound::StreamEnd {
                            stream_id: id,
                            error: Some(message),
                        },
                    );
                }
            }
        }
    }

    let mut progressed = false;
    loop {
        match classify(rs, id, &mut progressed) {
            Step::Idle => return progressed,
            Step::AwaitPending(p) => {
                let local = v8::Local::new(scope, &p);
                match local.state() {
                    v8::PromiseState::Pending => return progressed,
                    v8::PromiseState::Fulfilled => {
                        let value = local.result(scope);
                        let outcome = chunk_from_pull(scope, value);
                        settle(rs, id, outcome);
                        progressed = true;
                    }
                    v8::PromiseState::Rejected => {
                        let rejection = local.result(scope);
                        let message = error_message_from_value(scope, rejection)
                            .unwrap_or_else(|| "body stream failed".to_string());
                        settle(rs, id, Err(message));
                        progressed = true;
                    }
                }
            }
            Step::StartPull(body) => {
                let body_local = v8::Local::new(scope, &body);
                match start_body_pull(scope, body_local) {
                    Ok(promise) => {
                        if scope.is_execution_terminating() {
                            return progressed;
                        }
                        let global = v8::Global::new(scope, promise);
                        // SAFETY: same table contract as above.
                        let table = unsafe { &*rs.out_streams };
                        let mut slot = table.slot.borrow_mut();
                        if let Some(st) = slot.as_mut() {
                            if let Some(stream) = st.streams.get_mut(&id) {
                                stream.pending = Some(global);
                            }
                        }
                        // Let the caller checkpoint before re-checking.
                        return true;
                    }
                    Err(message) => {
                        if scope.is_execution_terminating() {
                            return progressed;
                        }
                        settle(rs, id, Err(message));
                        return true;
                    }
                }
            }
        }
    }
}

/// Deliver one `StreamChunk`/`StreamEnd` frame into the run's stream table:
/// resolve the pending sandbox read when one waits (crediting consumed
/// bytes), buffer otherwise. Ignores frames for unknown or cancelled streams
/// (benign races with a cancel already sent).
pub(super) fn deliver_stream_frame(
    scope: &mut v8::PinScope,
    streams: *const StreamTable,
    msg: Inbound,
) -> Result<(), RunError> {
    // SAFETY: the table outlives the run's events; transient borrow.
    let table = unsafe { &*streams };
    let mut slot = table.slot.borrow_mut();
    let Some(state) = slot.as_mut() else {
        return Ok(()); // no run armed (stray message) — drop it
    };

    match msg {
        Inbound::StreamChunk { stream_id, data } => {
            let Some(stream) = state.streams.get_mut(&stream_id) else {
                return Ok(());
            };
            if stream.cancelled {
                return Ok(());
            }
            if let Some(pending) = stream.pending.take() {
                let credit = data.len() as u32;
                send_stream_credit(state, stream_id, credit);
                drop(slot);
                let value = bytes_to_uint8array(scope, data);
                let resolver = v8::Local::new(scope, pending);
                match value {
                    Some(v) => {
                        resolver.resolve(scope, v.into());
                    }
                    None => {
                        let msg =
                            v8_error_value(scope, "[iso4] stream chunk could not be materialised");
                        resolver.reject(scope, msg);
                    }
                }
                return Ok(());
            }
            // Buffered: the host must stay inside the credit window; a small
            // slack of one max chunk absorbs frames in flight when a cancel
            // races the pump.
            let cap = types::STREAM_CREDIT_WINDOW_BYTES as usize
                + types::STREAM_CHUNK_MAX_BYTES as usize;
            if stream.buffered_bytes + data.len() > cap {
                return Err(RunError::Internal(
                    "stream credit window violated by the host".to_string(),
                ));
            }
            stream.buffered_bytes += data.len();
            stream.buffered.push_back(data);
            Ok(())
        }
        Inbound::StreamEnd { stream_id, error } => {
            let Some(stream) = state.streams.get_mut(&stream_id) else {
                return Ok(());
            };
            let outcome: Result<(), String> = match error {
                None => Ok(()),
                Some(message) => Err(message),
            };
            stream.ended = Some(outcome.clone());
            // A read waiting on an empty buffer observes the end immediately.
            if stream.buffered.is_empty() {
                if let Some(pending) = stream.pending.take() {
                    drop(slot);
                    let resolver = v8::Local::new(scope, pending);
                    match outcome {
                        Ok(()) => {
                            let null = v8::null(scope).into();
                            resolver.resolve(scope, null);
                        }
                        Err(message) => {
                            let msg = v8_error_value(
                                scope,
                                &format!("[iso4] body stream failed: {message}"),
                            );
                            resolver.reject(scope, msg);
                        }
                    }
                }
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// End-of-run stream cleanup: cancel every stream the host may still be
/// pumping, disarm the table, and clear the per-thread pointer. Idempotent.
pub(super) fn close_run_streams(streams: *const StreamTable) {
    // SAFETY: same contract as deliver_stream_frame.
    let table = unsafe { &*streams };
    let taken = table.slot.borrow_mut().take();
    if let Some(state) = taken {
        for (id, stream) in &state.streams {
            if stream.ended.is_none() && !stream.cancelled {
                send_stream_cancel(&state, *id, "run ended");
            }
        }
    }
}

/// Private-symbol key stamping a hydrated body-stream object with its stream
/// id — Rust-readable, guest-invisible, and how the outbound codec refuses to
/// serialize a stream back to the host.
const BODY_STREAM_ID_KEY: &str = "iso4::bodyStreamId";

/// Private marker stamped on EVERY `Iso4BodyStream` at construction (the JS
/// constructor calls the `stampStreamBody` native), so the codec can
/// recognize a stream body without consulting guest-mutable state.
const STREAM_BODY_MARK_KEY: &str = "iso4::streamBody";

fn body_stream_private<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    name: &str,
) -> Option<v8::Local<'s, v8::Private>> {
    let key = v8::String::new(scope, name)?;
    Some(v8::Private::for_api(scope, Some(key)))
}

/// The stream id a hydrated body-stream object was stamped with, if this is
/// one. Used by the outbound body check.
pub fn body_stream_id_of(scope: &mut v8::PinScope, value: v8::Local<v8::Value>) -> Option<u32> {
    let obj = v8::Local::<v8::Object>::try_from(value).ok()?;
    let key = body_stream_private(scope, BODY_STREAM_ID_KEY)?;
    let stamped = obj.get_private(scope, key)?;
    if !stamped.is_uint32() {
        return None;
    }
    stamped.uint32_value(scope)
}

/// Whether `value` is a guest body stream (`Iso4BodyStream`), by its
/// construction-time private stamp — never `instanceof` or duck typing.
pub fn is_stream_body(scope: &mut v8::PinScope, value: v8::Local<v8::Value>) -> bool {
    let Ok(obj) = v8::Local::<v8::Object>::try_from(value) else {
        return false;
    };
    let Some(key) = body_stream_private(scope, STREAM_BODY_MARK_KEY) else {
        return false;
    };
    obj.get_private(scope, key).is_some_and(|v| v.is_true())
}

/// `stampStreamBody(obj)` — native handed to the web runtime; the
/// `Iso4BodyStream` constructor stamps every instance with the private
/// marker [`is_stream_body`] checks.
pub(crate) fn stamp_stream_body_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let Ok(obj) = v8::Local::<v8::Object>::try_from(args.get(0)) else {
        return;
    };
    let Some(key) = body_stream_private(scope, STREAM_BODY_MARK_KEY) else {
        return;
    };
    let yes = v8::Boolean::new(scope, true);
    obj.set_private(scope, key, yes.into());
}

/// Build the sandbox-side body-stream object for a hydrated stream handle:
/// calls the factory the web runtime returned at install and stamps the
/// stream id under a private key. Runs during the rehydration walk (JS is
/// legal there).
pub fn make_body_stream<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    stream_id: u32,
) -> Option<v8::Local<'s, v8::Object>> {
    let global = scope.get_current_context().global(scope);
    let factory_key = body_stream_private(scope, crate::webtypes::BODY_STREAM_FACTORY_KEY)?;
    let factory =
        v8::Local::<v8::Function>::try_from(global.get_private(scope, factory_key)?).ok()?;
    let recv = v8::undefined(scope).into();
    let id_arg = v8::Integer::new_from_unsigned(scope, stream_id);
    let stream = factory.call(scope, recv, &[id_arg.into()])?;
    let stream = v8::Local::<v8::Object>::try_from(stream).ok()?;
    let id_key = body_stream_private(scope, BODY_STREAM_ID_KEY)?;
    let id_value = v8::Integer::new_from_unsigned(scope, stream_id);
    stream.set_private(scope, id_key, id_value.into())?;
    Some(stream)
}

/// `streamRead(streamId)` — native pull for the sandbox body stream: resolves
/// with a `Uint8Array` chunk, `null` at EOF, or rejects on a source failure.
/// Consumed bytes replenish the host's credit window.
pub(crate) fn stream_read_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let Some(run_table) = slot_table(scope) else {
        throw_v8_error(scope, "[iso4] stream read outside an instance");
        return;
    };
    let Some(resolver) = v8::PromiseResolver::new(scope) else {
        throw_v8_error(scope, "[iso4] stream read could not allocate");
        return;
    };
    rv.set(resolver.get_promise(scope).into());
    let stream_id = args.get(0).uint32_value(scope).unwrap_or(0);

    let state = attributed_state(scope, run_table);
    if state.is_null() {
        let msg = v8_error_value(scope, "[iso4] streamed bodies are only readable during a run");
        resolver.reject(scope, msg);
        return;
    }
    // SAFETY: entry pointers are valid for the remainder of the turn.
    let table = unsafe { &*state }.streams.as_ref();
    let mut slot = table.slot.borrow_mut();
    let Some(state) = slot.as_mut() else {
        drop(slot);
        let msg = v8_error_value(scope, "[iso4] streamed bodies are only readable during a run");
        resolver.reject(scope, msg);
        return;
    };
    let Some(stream) = state.streams.get_mut(&stream_id) else {
        drop(slot);
        let msg = v8_error_value(scope, "[iso4] unknown body stream");
        resolver.reject(scope, msg);
        return;
    };

    if let Some(chunk) = stream.buffered.pop_front() {
        stream.buffered_bytes -= chunk.len();
        let credit = chunk.len() as u32;
        let value = bytes_to_uint8array(scope, chunk);
        send_stream_credit(state, stream_id, credit);
        drop(slot);
        match value {
            Some(v) => {
                resolver.resolve(scope, v.into());
            }
            None => {
                let msg = v8_error_value(scope, "[iso4] stream chunk could not be materialised");
                resolver.reject(scope, msg);
            }
        }
        return;
    }
    if stream.cancelled {
        drop(slot);
        let null = v8::null(scope).into();
        resolver.resolve(scope, null);
        return;
    }
    match &stream.ended {
        Some(Ok(())) => {
            drop(slot);
            let null = v8::null(scope).into();
            resolver.resolve(scope, null);
        }
        Some(Err(message)) => {
            let message = message.clone();
            drop(slot);
            let msg = v8_error_value(scope, &format!("[iso4] body stream failed: {message}"));
            resolver.reject(scope, msg);
        }
        None => {
            if stream.pending.is_some() {
                drop(slot);
                let msg = v8_error_value(scope, "[iso4] a read is already pending on this stream");
                resolver.reject(scope, msg);
                return;
            }
            stream.pending = Some(v8::Global::new(scope, resolver));
        }
    }
}

/// `streamCancel(streamId, reason)` — the sandbox is done with the body: the
/// host is told to stop pumping and release the source; buffered chunks drop.
pub(crate) fn stream_cancel_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let Some(run_table) = slot_table(scope) else {
        rv.set_undefined();
        return;
    };
    let state = attributed_state(scope, run_table);
    if state.is_null() {
        rv.set_undefined();
        return;
    }
    // SAFETY: entry pointers are valid for the remainder of the turn.
    let table = unsafe { &*state }.streams.as_ref();
    let stream_id = args.get(0).uint32_value(scope).unwrap_or(0);
    let reason = args
        .get(1)
        .to_string(scope)
        .map(|s| s.to_rust_string_lossy(scope))
        .unwrap_or_default();
    let mut slot = table.slot.borrow_mut();
    let mut pending_read: Option<v8::Global<v8::PromiseResolver>> = None;
    if let Some(state) = slot.as_mut() {
        let mut notify_host = false;
        if let Some(stream) = state.streams.get_mut(&stream_id) {
            if !stream.cancelled && stream.ended.is_none() {
                stream.cancelled = true;
                stream.buffered.clear();
                stream.buffered_bytes = 0;
                notify_host = true;
            }
            pending_read = stream.pending.take();
        }
        if notify_host {
            send_stream_cancel(state, stream_id, &reason);
        }
    }
    drop(slot);
    if let Some(pending) = pending_read {
        let resolver = v8::Local::new(scope, pending);
        let null = v8::null(scope).into();
        resolver.resolve(scope, null);
    }
    rv.set_undefined();
}

/// Copy chunk bytes into a fresh sandbox `Uint8Array`.
fn bytes_to_uint8array<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    bytes: Vec<u8>,
) -> Option<v8::Local<'s, v8::Uint8Array>> {
    let len = bytes.len();
    let store = v8::ArrayBuffer::new_backing_store_from_vec(bytes).make_shared();
    let buffer = v8::ArrayBuffer::with_backing_store(scope, &store);
    v8::Uint8Array::new(scope, buffer, 0, len)
}
