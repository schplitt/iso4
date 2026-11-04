//! Per-run state: the run table that keys everything a native callback
//! reaches by the owning run, the per-instance load meter, and the CPED
//! rider that carries a run's token through its continuations.
//!
//! One instance serves several in-flight runs, so a turn executes on behalf
//! of exactly one run ([`CURRENT_RUN_TOKEN`]) and continuations created by a
//! run carry its token in the rider, so work resumed during another run's
//! turn still attributes to its creator.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use crate::link::{Addr, Observer};
use crate::v8::PendingWork;
use crate::v8::bridge::GlobalCallbackData;
use crate::v8::isolate::LogBuffers;
use crate::v8::streams::{OutStreamTable, StreamTable};
use crate::v8::timers::TimerTable;

/// The instance's run table, held in an isolate slot so the natives baked
/// into the startup snapshot carry no per-instance pointer.
#[derive(Clone, Copy)]
pub(crate) struct RunTableSlot(pub(crate) *const RunTable);

pub(super) fn slot_table<'s>(scope: &mut v8::PinScope<'s, '_>) -> Option<&'s RunTable> {
    // SAFETY: the slot points at the instance-lifetime RunTable Box, which
    // outlives every callback of its isolate.
    scope
        .get_slot::<RunTableSlot>()
        .filter(|slot| !slot.0.is_null())
        .map(|slot| unsafe { &*slot.0 })
}

thread_local! {
    /// The run whose turn is executing on this thread. 0 = none (between
    /// turns, prefix warm-up). Fallback attribution when no CPED rider is
    /// present; the gate that refuses between-turn callback activity.
    pub(super) static CURRENT_RUN_TOKEN: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };

    /// The run table of the instance executing on this thread, for callback
    /// paths that have no `scope`/External to reach it through (the codec's
    /// rehydration walk). Null outside turns.
    pub(super) static RUN_TABLE_PTR: std::cell::Cell<*const RunTable> =
        const { std::cell::Cell::new(std::ptr::null()) };

    /// Runs whose timer set changed during the current turn: a CPED-rider
    /// continuation can arm a CO-RESIDENT's timer, invisible to the
    /// owner-only post-turn re-queue — the loop drains this after each turn.
    pub(super) static TIMERS_TOUCHED: RefCell<Vec<u64>> = const { RefCell::new(Vec::new()) };
}

/// Per-instance signals written by the owner loop and read by the warm
/// registry: thread load for join routing, settled heap for eviction.
pub struct InstanceLoad {
    /// Thread utilization over the last closed window, per-mille.
    util_permille: std::sync::atomic::AtomicU32,
    /// When that window closed (ms since [`load_now_ms`]'s epoch). A stale
    /// window means the loop is parked — utilization is genuinely ~0.
    closed_at_ms: std::sync::atomic::AtomicU64,
    /// When the turn running right now started, `u64::MAX` between turns.
    /// Windows close only between turns, so a long turn would otherwise
    /// read as idle and attract more joins.
    turn_open_since_ms: std::sync::atomic::AtomicU64,
    /// Heap measured after the idle settle GC; 0 = none since the last
    /// release. Fresher than the release-time reading, which still counts
    /// the last run's garbage.
    settled_heap_bytes: std::sync::atomic::AtomicU64,
}

/// Utilization windows: closed every 100 ms of wall time; a reading older
/// than 500 ms reports 0 (parked loop).
pub(super) const LOAD_WINDOW_MS: u64 = 100;
const LOAD_STALE_MS: u64 = 500;

/// Milliseconds on the process-wide load epoch (cross-thread comparable).
pub fn load_now_ms() -> u64 {
    static ANCHOR: OnceLock<std::time::Instant> = OnceLock::new();
    ANCHOR.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64
}

impl InstanceLoad {
    pub fn new() -> Self {
        Self {
            util_permille: std::sync::atomic::AtomicU32::new(0),
            closed_at_ms: std::sync::atomic::AtomicU64::new(0),
            turn_open_since_ms: std::sync::atomic::AtomicU64::new(u64::MAX),
            settled_heap_bytes: std::sync::atomic::AtomicU64::new(0),
        }
    }

    pub(super) fn publish_settled_heap(&self, bytes: u64) {
        self.settled_heap_bytes
            .store(bytes, std::sync::atomic::Ordering::Release);
    }

    /// The settled reading, or 0 when none is current.
    pub fn settled_heap_bytes(&self) -> u64 {
        self.settled_heap_bytes
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// A run finished on the instance: whatever was settled is spent.
    pub fn clear_settled_heap(&self) {
        self.publish_settled_heap(0);
    }

    pub(super) fn close_window(&self, permille: u32) {
        self.util_permille
            .store(permille, std::sync::atomic::Ordering::Release);
        self.closed_at_ms
            .store(load_now_ms(), std::sync::atomic::Ordering::Release);
    }

    pub(super) fn open_turn(&self, at_ms: u64) {
        self.turn_open_since_ms
            .store(at_ms, std::sync::atomic::Ordering::Release);
    }

    pub(super) fn close_turn(&self) {
        self.turn_open_since_ms
            .store(u64::MAX, std::sync::atomic::Ordering::Release);
    }

    /// Recent utilization for routing; 0 when the last window is stale.
    pub fn current_util_permille(&self) -> u32 {
        self.util_permille_at(load_now_ms())
    }

    fn util_permille_at(&self, now_ms: u64) -> u32 {
        let open = self
            .turn_open_since_ms
            .load(std::sync::atomic::Ordering::Acquire);
        if now_ms.saturating_sub(open) >= LOAD_WINDOW_MS {
            // A turn holding the thread for a whole window IS saturation,
            // whatever the last closed window measured.
            return UTILIZATION_SATURATED_PERMILLE;
        }
        let at = self.closed_at_ms.load(std::sync::atomic::Ordering::Acquire);
        if now_ms.saturating_sub(at) > LOAD_STALE_MS {
            return 0;
        }
        self.util_permille.load(std::sync::atomic::Ordering::Acquire)
    }
}

/// Full utilization: reported for a turn that outlasts a window.
const UTILIZATION_SATURATED_PERMILLE: u32 = 1000;

impl Default for InstanceLoad {
    fn default() -> Self {
        Self::new()
    }
}

/// Process-wide run-token allocator. Tokens are instance-local identities on
/// the wire-facing side but allocated globally so no two live runs anywhere
/// share one (wire run ids can collide across connections; tokens cannot).
/// Starts at 1 — 0 means "no run".
pub fn alloc_run_token() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Everything one in-flight run owns on its instance. Boxed in the table so
/// the address is stable across map rehashes; pointers into an entry are
/// derived and used only within one turn/callback invocation, and entries
/// are inserted/removed only between turns (no JS on the stack), so such a
/// pointer never outlives its entry.
pub(super) struct RunCallState {
    /// Console output, capped per this run's limits.
    pub(super) logs: Box<RefCell<LogBuffers>>,
    /// `waitUntil` registrations (armed at the setup-to-run boundary).
    pub(super) pending: Box<PendingWork>,
    /// Streamed-body registry (armed per run).
    pub(super) streams: Box<StreamTable>,
    /// Outbound streamed-body registry (armed per run) — result bodies the
    /// host reads chunk by chunk (#128).
    pub(super) out_streams: Box<OutStreamTable>,
    /// Pending `setTimeout` registrations; dropped with the entry, so a
    /// settled run's timer can never fire into a later run.
    pub(super) timers: Box<RefCell<TimerTable>>,
    /// Per-stub bridge bindings for THIS run — the per-call half of what a
    /// stub `Function` needs; the instance-lifetime half is the
    /// [`super::bridge::StubSlot`] its External points at.
    /// Rc'd so the bridge callback can take a per-invocation handle with one
    /// refcount bump instead of cloning the whole binding (two Strings and a
    /// fistful of Arcs).
    pub(super) stubs: RefCell<HashMap<String, std::rc::Rc<GlobalCallbackData>>>,
    /// The side channel console lines are copied to as they happen.
    pub(super) observer: Arc<dyn Observer>,
    /// This run's address on observer messages.
    pub(super) addr: Addr,
}

impl RunCallState {
    pub(super) fn new(
        max_stdout_bytes: u32,
        max_stderr_bytes: u32,
        observer: Arc<dyn Observer>,
        addr: Addr,
    ) -> Box<Self> {
        Box::new(Self {
            logs: Box::new(RefCell::new(LogBuffers {
                max_stdout_bytes,
                max_stderr_bytes,
                ..LogBuffers::default()
            })),
            pending: PendingWork::boxed(),
            streams: StreamTable::boxed(),
            out_streams: OutStreamTable::boxed(),
            timers: TimerTable::boxed(),
            stubs: RefCell::new(HashMap::new()),
            observer,
            addr,
        })
    }
}

/// Instance-lifetime registry of in-flight runs, reachable from native
/// callbacks through an `External` (stable Box address, the `LogBuffers`
/// pattern generalized).
pub struct RunTable {
    pub(super) runs: RefCell<HashMap<u64, Box<RunCallState>>>,
    /// Console output produced outside any run — prefix warm-up — delivered
    /// on the cold-start call (the old `warmup_logs_pending` flow).
    pub(super) setup_logs: RefCell<LogBuffers>,
}

impl RunTable {
    pub(super) fn boxed() -> Box<Self> {
        Box::new(Self {
            runs: RefCell::new(HashMap::new()),
            setup_logs: RefCell::new(LogBuffers::default()),
        })
    }

    /// Pointer to a live run's state, or null. The pointer is valid for the
    /// remainder of the current turn (entries are only removed between
    /// turns); callers must not hold it longer.
    pub(super) fn state_ptr(&self, token: u64) -> *const RunCallState {
        self.runs
            .borrow()
            .get(&token)
            .map_or(std::ptr::null(), |b| std::ptr::addr_of!(**b))
    }
}

/// The run native callbacks attribute to: the CPED rider's creator token
/// when the running continuation carries one AND that run is still live,
/// else the current turn owner. (A stale continuation of a finished run —
/// driven during a later run's checkpoint on a warm instance — attributes
/// to the turn owner, exactly the pre-table behavior of the shared buffers.)
///
/// Returns the run's state pointer (null when the attributed run has no
/// live entry — setup code, or a stale continuation whose turn owner
/// already concluded), resolved in ONE table borrow: the old
/// token-then-`state_ptr` pair cost a second hash lookup per bridge call
/// and console line.
///
/// The pointer is valid for the remainder of the current turn (entries are
/// only removed between turns); callers must not hold it longer.
pub(super) fn attributed_state(scope: &mut v8::PinScope, table: &RunTable) -> *const RunCallState {
    attributed_token_and_state(scope, table).1
}

/// [`attributed_state`] plus the run's token, in the same table borrow.
pub(super) fn attributed_token_and_state(
    scope: &mut v8::PinScope,
    table: &RunTable,
) -> (u64, *const RunCallState) {
    let current = CURRENT_RUN_TOKEN.with(|c| c.get());
    let cped = scope.get_continuation_preserved_embedder_data();
    let rider = rider_parts(scope, cped).map(|(token, _)| token);
    let runs = table.runs.borrow();
    if let Some(token) = rider {
        if token != current {
            // A live rider run wins; a stale rider falls back to the owner.
            if let Some(entry) = runs.get(&token) {
                return (token, std::ptr::addr_of!(**entry));
            }
        }
    }
    (
        current,
        runs.get(&current)
            .map_or(std::ptr::null(), |b| std::ptr::addr_of!(**b)),
    )
}

// ── The CPED rider ───────────────────────────────────────────────────────────
//
// The CPED slot holds `[token, frame]`: the owning run's token beside the
// guest's AsyncLocalStorage frame. V8 saves and restores CPED per
// continuation, so a continuation created by run A attributes to A even
// when it executes during run B's turn. Guests never see the rider: the
// `node:async_hooks` natives unwrap and wrap it.

/// Build a rider `[token, inner]`.
pub(super) fn make_rider<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    token: u64,
    inner: v8::Local<v8::Value>,
) -> v8::Local<'s, v8::Array> {
    let rider = v8::Array::new(scope, 2);
    let token_val = v8::Number::new(scope, token as f64);
    rider.set_index(scope, 0, token_val.into());
    rider.set_index(scope, 1, inner);
    rider
}

/// Split a CPED value into `(token, inner)` when it is a rider.
pub(super) fn rider_parts<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    value: v8::Local<v8::Value>,
) -> Option<(u64, v8::Local<'s, v8::Value>)> {
    let arr = v8::Local::<v8::Array>::try_from(value).ok()?;
    if arr.length() != 2 {
        return None;
    }
    let token = arr.get_index(scope, 0)?.number_value(scope)?;
    let inner = arr.get_index(scope, 1)?;
    Some((token as u64, inner))
}

/// Install this run's rider at turn entry: continuations created from here
/// carry the token. The previous rider needs no restoring — turn exit clears
/// [`CURRENT_RUN_TOKEN`] and the next turn overwrites the slot.
pub(super) fn install_cped_rider(scope: &mut v8::PinScope, token: u64) {
    let undefined = v8::undefined(scope).into();
    let rider = make_rider(scope, token, undefined);
    scope.set_continuation_preserved_embedder_data(rider.into());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_turn_outlasting_a_window_reads_as_saturated() {
        let load = InstanceLoad::new();
        load.close_window(300);
        assert_eq!(load.util_permille_at(10), 300, "between turns: the window");
        load.open_turn(10);
        assert_eq!(load.util_permille_at(10 + LOAD_WINDOW_MS - 1), 300);
        assert_eq!(
            load.util_permille_at(10 + LOAD_WINDOW_MS),
            UTILIZATION_SATURATED_PERMILLE,
            "one turn has held the thread for a whole window",
        );
        load.close_turn();
        assert_eq!(load.util_permille_at(10 + LOAD_WINDOW_MS), 300);
    }
}
