//! The instance loop: one owner thread serving a warm instance's queue of
//! calls, suspended runs, deadlines, completions, and the taint sweep.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io;
use std::sync::OnceLock;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::errors::{failure, reason_error, termination_or};
use crate::link::{Addr, HostLink, Inbound, LinkError, Outbound};
use crate::types;
use crate::v8::bridge::{BridgeCallLog, install_prefix_bridge_placeholders};
use crate::v8::guard::GuardTarget;
use crate::v8::isolate::{LogBuffers, TerminationReason, heap_used, idle_settle_after, settle_heap};
use crate::v8::runloop::{
    CallPhaseCtx, LoopEvent, RunEntry, RunFacts, RunPhase, RunState, ScanOutcome, begin_call,
    boundary_close, boundary_deadline, conclude_grace_wall, counted_wall_ms, earliest_timer,
    elapsed_ms, enter_turn, grace_wall_due, handle_loop_event, instance_tainted, leave_turn,
    next_deadline, stream_idle_turn,
};
use crate::v8::runtable::{LOAD_WINDOW_MS, RUN_TABLE_PTR, RunCallState, TIMERS_TOUCHED};
use crate::v8::streams::{close_out_streams, close_run_streams, out_idle_deadline};
use crate::v8::timers::{TimerTable, timer_turn};
use crate::v8::{
    CallJob, CallOutcome, CpuBudget, FailureOutput, GraceReport, GraceStatus, InstanceCore,
    InstanceLoad, OomRun, OutStreamTable, Output, PendingWork, ResetCause, RoutedEvent, RunError,
    RunEvent, RunIo, RunTable, StreamTable, alloc_run_token, load_now_ms, set_oom_run,
};

// ── The instance loop ────────────────────────────────────────────────────────

/// One suspended run on the instance loop.
struct LiveRun {
    rs: RunState,
    job: Box<CallJob>,
}

/// Finish one run: stamp its telemetry, close its streams, drop its table
/// entry, then release it and answer the host (`finish_job`).
fn conclude_run(
    isolate: &mut v8::OwnedIsolate,
    run_table: &RunTable,
    mut live: LiveRun,
    mut result: Result<Output, FailureOutput>,
    tainted: bool,
) {
    let cpu_time_ms = live.rs.cpu_budget.elapsed_ms_precise();
    let wall_time_ms = counted_wall_ms(&live.rs);
    let records = live
        .rs
        .bridge_log
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .finalize(elapsed_ms(live.rs.start));
    match &mut result {
        Ok(output) => {
            output.cpu_time_ms = cpu_time_ms;
            output.wall_time_ms = wall_time_ms;
            output.bridge_calls = records;
        }
        Err(f) => {
            f.cpu_time_ms = cpu_time_ms;
            f.wall_time_ms = wall_time_ms;
            f.bridge_calls = records;
        }
    }
    close_run_streams(live.rs.streams);
    close_out_streams(live.rs.out_streams, "run ended");
    run_table.runs.borrow_mut().remove(&live.rs.token);
    // Between turns: nothing may carry an unconsumed terminate into later work.
    isolate.cancel_terminate_execution();
    let outcome = CallOutcome {
        result,
        tainted,
        heap_used_bytes: heap_used(isolate),
        prefix_failed: false,
    };
    finish_job(&mut live.job, live.rs.link.take(), outcome);
}

/// Conclude a job through [`finish_run`], its release hook as the
/// bookkeeping.
pub fn finish_job(job: &mut CallJob, link: Option<Arc<dyn HostLink>>, outcome: CallOutcome) {
    let run_id = job.epilogue.map(|e| e.run_id).unwrap_or(0);
    let msg = link.as_ref().map(|_| {
        completion_message(
            &outcome.result,
            Some(outcome.heap_used_bytes),
            outcome.prefix_failed,
        )
    });
    let release = job.release.take();
    finish_run(link.as_deref(), run_id, msg, || {
        if let Some(release) = release {
            release(outcome);
        }
    });
}

/// Conclude a run: the frontend drops its route, the registry bookkeeping
/// runs (the instance is back before the host can see the answer), then the
/// completion goes out.
pub fn finish_run(
    link: Option<&dyn HostLink>,
    run_id: u32,
    msg: Option<Outbound>,
    release: impl FnOnce(),
) {
    if let Some(link) = link {
        link.finished(Addr::Run(run_id));
    }
    release();
    if let (Some(link), Some(msg)) = (link, msg) {
        send_completion(link, run_id, msg);
    }
}

/// A run's completion as the host receives it: the final `Result`, or the
/// `RunComplete` when the early Result already went out during grace.
pub fn completion_message(
    result: &Result<Output, FailureOutput>,
    heap_used_bytes: Option<u64>,
    prefix_stage: bool,
) -> Outbound {
    match result {
        Ok(output) => match &output.background {
            Some(bg) if bg.early_result_sent => Outbound::RunComplete { report: bg.clone() },
            _ => Outbound::Result {
                completion: types::RunCompletion::Success(types::RunSuccessPayload {
                    exports: output.exports.clone(),
                    skipped_exports: output.skipped_exports.clone(),
                    stdout: output.stdout.clone(),
                    stderr: output.stderr.clone(),
                    duration_ms: output.duration_ms,
                    wall_time_ms: output.wall_time_ms,
                    cpu_time_ms: output.cpu_time_ms,
                    bridge_calls: output.bridge_calls.clone(),
                    heap_used_bytes,
                    background_flags: 0,
                }),
            },
        },
        Err(failure) => Outbound::Result {
            completion: types::RunCompletion::Failure(types::RunFailurePayload {
                error: types::RunErrorPayload {
                    prefix_stage,
                    ..failure.error.to_payload()
                },
                stdout: failure.stdout.clone(),
                stderr: failure.stderr.clone(),
                duration_ms: failure.duration_ms,
                wall_time_ms: failure.wall_time_ms,
                cpu_time_ms: failure.cpu_time_ms,
                bridge_calls: failure.bridge_calls.clone(),
                heap_used_bytes,
            }),
        },
    }
}

/// Send a completion, substituting a minimal one when the host cannot carry
/// it: a guest-authored oversized rejection message (or an unbounded record
/// list under `maxBridgeCalls: 0`) costs its telemetry, never the answer.
/// Other failures are logged: the run's own events report the dead host.
pub fn send_completion(link: &dyn HostLink, run_id: u32, msg: Outbound) {
    let report_status = match &msg {
        Outbound::RunComplete { report } => Some(report.status),
        _ => None,
    };
    let outcome = match link.send(Addr::Run(run_id), msg) {
        Err(LinkError::TooLarge(detail)) if report_status.is_none() => {
            eprintln!(
                "[iso4-v8] Result for run {run_id} too large to send ({detail}) — \
                 replacing it with ERR_INTERNAL"
            );
            // stdout/stderr are dropped too: unbounded, and this payload has
            // to be small by construction.
            let completion = types::RunCompletion::Failure(types::RunFailurePayload::bare(
                RunError::Internal(format!(
                    "the run's result was too large to send to the host ({detail}). \
                     A thrown error's message, stack and properties are reported \
                     verbatim, so a very large one cannot be delivered."
                ))
                .to_payload(),
            ));
            link.send(Addr::Run(run_id), Outbound::Result { completion })
        }
        Err(LinkError::TooLarge(detail)) => {
            eprintln!(
                "[iso4-v8] RunComplete for run {run_id} too large to send ({detail}) — \
                 substituting a minimal report"
            );
            let report = GraceReport {
                status: report_status.unwrap_or(GraceStatus::Settled),
                duration_ms: 0.0,
                wall_time_ms: 0.0,
                cpu_time_ms: 0.0,
                stdout: Vec::new(),
                stderr: Vec::new(),
                bridge_calls: Vec::new(),
                error: Some((
                    "Error".to_string(),
                    "the grace report was too large to send; its telemetry was dropped"
                        .to_string(),
                )),
                early_result_sent: true,
            };
            link.send(Addr::Run(run_id), Outbound::RunComplete { report })
        }
        other => other,
    };
    if let Err(e) = outcome {
        eprintln!("[iso4-v8] failed to send completion for run {run_id}: {e}");
    }
}

/// The multi-run loop's deadline heap: `(deadline, run token)`, nearest
/// first. Lazily deleted — an entry is stale when it no longer equals its
/// run's [`next_deadline`]. Each turn re-queues at most one entry per
/// touched run, so the heap grows with turns, never with pending-timer
/// count.
type DeadlineHeap = std::collections::BinaryHeap<std::cmp::Reverse<(std::time::Instant, u64)>>;

/// The nearest live deadline, discarding stale heap tops on the way.
fn peek_valid_deadline(
    live: &[LiveRun],
    deadlines: &mut DeadlineHeap,
) -> Option<(std::time::Instant, u64)> {
    loop {
        let std::cmp::Reverse((at, token)) = *deadlines.peek()?;
        let valid = live
            .iter()
            .find(|l| l.rs.token == token)
            .is_some_and(|l| next_deadline(&l.rs) == Some(at));
        if valid {
            return Some((at, token));
        }
        deadlines.pop();
    }
}

/// Re-queue deadlines for runs whose timer set changed during the last
/// turn (see [`TIMERS_TOUCHED`]); duplicates are harmless (lazy deletion).
fn drain_touched_timers(live: &[LiveRun], deadlines: &mut DeadlineHeap) {
    TIMERS_TOUCHED.with(|v| {
        for token in v.borrow_mut().drain(..) {
            if let Some(l) = live.iter().find(|l| l.rs.token == token) {
                if let Some(at) = next_deadline(&l.rs) {
                    deadlines.push(std::cmp::Reverse((at, token)));
                }
            }
        }
    });
}

/// Serve every deadline at or before `cutoff`: due TIMERS fire (a real
/// turn — guest JS, so taint can arise; `true` = tainted, caller sweeps
/// and returns), expired BOUNDARIES conclude their runs (no JS). A timer
/// tying with its run's boundary fires first.
///
/// The cutoff is what makes deadlines fire in ARRIVAL order rather than
/// processing order: the event paths pass the event's demux arrival stamp,
/// so a deadline concludes its run only once every event that arrived
/// before it has been delivered — a settling frame that reached the demux
/// in time is never beaten by the deadline just because the loop was busy —
/// while events arriving after the deadline cannot delay it. The idle path
/// (select timeout) passes `now`: the queue is empty at that moment, so
/// nothing arrived-but-undelivered exists.
#[must_use]
fn sweep_expired_deadlines(
    core: &mut InstanceCore,
    live: &mut Vec<LiveRun>,
    deadlines: &mut DeadlineHeap,
    cutoff: std::time::Instant,
) -> bool {
    while let Some((at, token)) = peek_valid_deadline(live, deadlines) {
        if at > cutoff {
            break;
        }
        deadlines.pop();
        let idx = live
            .iter()
            .position(|l| l.rs.token == token)
            .expect("peek_valid_deadline checked liveness");
        // Which deadline surfaced: the run's earliest timer, or its
        // phase boundary.
        if earliest_timer(&live[idx].rs) == Some(at) {
            let mut l = live.swap_remove(idx);
            note_turn_owner(&l.job);
            // The wait ended at the timer's due instant, not its late fire.
            enter_turn(&mut l.rs, at);
            let outcome = {
                let facts = RunFacts {
                    call: Some(&l.job.call),
                    reason: &core.reason,
                    guard: &core.guard,
                    cancel_handle: core.guard.handle(),
                };
                timer_turn(&mut core.isolate, &core.context, &facts, &mut l.rs)
            };
            let tainted_now = instance_tainted(core);
            match outcome {
                ScanOutcome::Finished(result) => {
                    conclude_run(&mut core.isolate, &core.run_table, l, *result, tainted_now);
                }
                ScanOutcome::Continue => {
                    if tainted_now {
                        // Bare terminate mid-turn: classify and conclude
                        // here (the event arm's rule).
                        let failure = l.rs.fail(reason_error(&core.reason));
                        conclude_run(&mut core.isolate, &core.run_table, l, Err(failure), true);
                    } else {
                        leave_turn(&mut l.rs);
                        if let Some(next) = next_deadline(&l.rs) {
                            deadlines.push(std::cmp::Reverse((next, token)));
                        }
                        live.push(l);
                    }
                }
            }
            drain_touched_timers(live, deadlines);
            if tainted_now {
                return true;
            }
            continue;
        }
        let mut l = live.swap_remove(idx);
        if matches!(l.rs.phase, RunPhase::Grace(_)) {
            let now = std::time::Instant::now();
            if let Some(d) = out_idle_deadline(&l.rs).filter(|d| *d <= now) {
                // Stream idle allowance: a real turn (guest cancel cleanup).
                note_turn_owner(&l.job);
                enter_turn(&mut l.rs, d);
                let outcome = {
                    let facts = RunFacts {
                        call: Some(&l.job.call),
                        reason: &core.reason,
                        guard: &core.guard,
                        cancel_handle: core.guard.handle(),
                    };
                    stream_idle_turn(&mut core.isolate, &core.context, &facts, &mut l.rs)
                };
                let tainted_now = instance_tainted(core);
                match outcome {
                    ScanOutcome::Finished(result) => {
                        conclude_run(&mut core.isolate, &core.run_table, l, *result, tainted_now);
                    }
                    ScanOutcome::Continue => {
                        if tainted_now {
                            let failure = l.rs.fail(reason_error(&core.reason));
                            conclude_run(&mut core.isolate, &core.run_table, l, Err(failure), true);
                        } else {
                            leave_turn(&mut l.rs);
                            if let Some(next) = next_deadline(&l.rs) {
                                deadlines.push(std::cmp::Reverse((next, token)));
                            }
                            live.push(l);
                        }
                    }
                }
                drain_touched_timers(live, deadlines);
                if tainted_now {
                    return true;
                }
                continue;
            }
            // waitUntil wall: bookkeeping — the phase may stay open for
            // streams.
            if grace_wall_due(&l.rs, now) {
                // Counted grace time reaches its cap exactly here.
                note_turn_owner(&l.job);
                enter_turn(&mut l.rs, at);
                match conclude_grace_wall(&mut l.rs) {
                    Some(output) => {
                        conclude_run(&mut core.isolate, &core.run_table, l, Ok(output), false);
                    }
                    None => {
                        leave_turn(&mut l.rs);
                        if let Some(next) = next_deadline(&l.rs) {
                            deadlines.push(std::cmp::Reverse((next, token)));
                        }
                        live.push(l);
                    }
                }
                continue;
            }
            // Stale entry (activity moved the deadline): requeue.
            if let Some(next) = next_deadline(&l.rs) {
                deadlines.push(std::cmp::Reverse((next, token)));
            }
            live.push(l);
            continue;
        }
        // Counted wall reaches its cap exactly at the boundary instant.
        note_turn_owner(&l.job);
        enter_turn(&mut l.rs, at);
        let result = boundary_deadline(&mut l.rs);
        conclude_run(&mut core.isolate, &core.run_table, l, result, false);
    }
    false
}

/// Serve one instance: the per-instance turn loop (#125). Receives from the
/// instance's ONE ordered channel — jobs, run-tagged events, and the
/// registry's Retire all ride it (#170) — bounded by the nearest per-run
/// deadline from a heap; each message is one turn. The demux enqueues a
/// run's job before it can route any of that run's frames, so channel FIFO
/// guarantees a run is live before its events are delivered: an event whose
/// token is not live is LATE (its run concluded), never early. Any number
/// of runs can be suspended at once.
///
/// Deadlines fire in ARRIVAL order, not processing order (see
/// [`sweep_expired_deadlines`]): each delivered event first concludes the
/// deadlines that expired before it arrived, and an idle recv timing out
/// concludes everything expired as of now. So a backlog of frames that
/// arrived after a run's wall cannot starve that deadline, and a settling
/// frame that arrived in time is never beaten by it.
///
/// Returns when a Retire message has been seen and no runs are in flight,
/// or when a turn is interrupted mid-JS — the taint path: the turn owner
/// fails with its classified error and every other in-flight run fails with
/// `ERR_INSTANCE_RESET` carrying the cause class and the culprit's wire run
/// id (E1 rulings 3+4). The instance dies with the loop either way.
pub fn serve_instance(
    core: &mut InstanceCore,
    prefix_globals: &[types::HostGlobalDef],
    imports: &[types::ImportBinding],
    first: Box<CallJob>,
    events: &crossbeam_channel::Receiver<RoutedEvent>,
    load: &InstanceLoad,
) {
    let mut live: Vec<LiveRun> = Vec::new();
    let mut deadlines = DeadlineHeap::new();
    let mut retired = false;
    // Whether the current idle stretch has had its settle GC.
    let mut settled = false;
    // Utilization bookkeeping: busy time per wall window, published for the
    // registry's join routing.
    let mut window_start = std::time::Instant::now();
    let mut busy_nanos: u64 = 0;
    // Built once; `Select` parks without `recv()`'s spin/yield backoff,
    // which loses on request-gap-sized waits.
    let mut sel = crossbeam_channel::Select::new();
    sel.recv(events);

    // The job that triggered instance creation, then the loop.
    load.open_turn(load_now_ms());
    let first_tainted =
        dispatch_job(core, prefix_globals, imports, &mut live, &mut deadlines, first);
    load.close_turn();
    if first_tainted {
        taint_sweep(core, &mut live, events);
        return;
    }

    loop {
        if live.is_empty() && retired {
            return;
        }
        set_oom_run(OomRun::None);

        let received = {
            let timeout = peek_valid_deadline(&live, &mut deadlines)
                .map(|(at, _)| at.saturating_duration_since(std::time::Instant::now()))
                // Nothing pending and the last run's garbage still held:
                // wake once to settle the heap, then park indefinitely.
                .or_else(|| (live.is_empty() && !settled).then(idle_settle_after).flatten());
            let oper = match timeout {
                Some(t) => sel.select_timeout(t).ok(),
                None => Some(sel.select()),
            };
            match oper {
                None => Err(crossbeam_channel::RecvTimeoutError::Timeout),
                Some(op) => op
                    .recv(events)
                    .map_err(|_| crossbeam_channel::RecvTimeoutError::Disconnected),
            }
        };
        if received.is_ok() {
            settled = false;
        }

        let turn_started = std::time::Instant::now();
        load.open_turn(load_now_ms());
        match received {
            Err(crossbeam_channel::RecvTimeoutError::Timeout) if live.is_empty() => {
                settled = true;
                settle_heap(&mut core.isolate);
                load.publish_settled_heap(heap_used(&mut core.isolate));
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                // The recv timed out: the channel was empty for the whole
                // window, so nothing arrived-but-undelivered can beat the
                // due deadline.
                if sweep_expired_deadlines(
                    core,
                    &mut live,
                    &mut deadlines,
                    std::time::Instant::now(),
                ) {
                    taint_sweep(core, &mut live, events);
                    return;
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                // Every sender is gone. The handle's Retire precedes its own
                // sender dropping, so runs still live here mean a panicked
                // feeder dropped its routes without the usual connection-lost
                // delivery. Nothing can ever wake them: fail them cleanly
                // instead of stranding them.
                while let Some(mut l) = live.pop() {
                    let result = boundary_close(
                        &mut l.rs,
                        io::Error::new(io::ErrorKind::BrokenPipe, "demux gone"),
                    );
                    conclude_run(&mut core.isolate, &core.run_table, l, result, false);
                }
                return;
            }
            Ok(RoutedEvent {
                event: RunEvent::Retire,
                ..
            }) => {
                retired = true;
            }
            Ok(RoutedEvent {
                event: RunEvent::Job(msg),
                at,
                ..
            }) => {
                // Deadlines/timers due before this job arrived fire first —
                // overdue work beats admitting new runs.
                if sweep_expired_deadlines(core, &mut live, &mut deadlines, at) {
                    taint_sweep(core, &mut live, events);
                    return;
                }
                if dispatch_job(core, prefix_globals, imports, &mut live, &mut deadlines, msg) {
                    taint_sweep(core, &mut live, events);
                    return;
                }
            }
            Ok(RoutedEvent { token, event, at }) => {
                // Deadlines that expired BEFORE this event arrived precede
                // it; anything later must not delay it (or be delayed by
                // it).
                if sweep_expired_deadlines(core, &mut live, &mut deadlines, at) {
                    taint_sweep(core, &mut live, events);
                    return;
                }
                let Some(idx) = live.iter().position(|l| l.rs.token == token) else {
                    // FIFO + the demux's job-before-frames order make this
                    // LATE by construction: the run concluded while the
                    // event sat in the queue. Discard like the demux's own
                    // late-frame rule — but loudly (silence hid #170).
                    match &event {
                        RunEvent::Host(msg) => eprintln!(
                            "[iso4-v8] discarding late {msg} (run already concluded)"
                        ),
                        RunEvent::Job(_) | RunEvent::Retire => {}
                    }
                    // Discarding is the whole turn; an unclosed one reads
                    // as 100% busy until the next event.
                    load.close_turn();
                    continue;
                };
                let mut l = live.swap_remove(idx);
                let event = match event {
                    RunEvent::Host(Inbound::ConnectionLost { detail }) => {
                        LoopEvent::SourceClosed(io::Error::new(
                            io::ErrorKind::BrokenPipe,
                            detail.unwrap_or_else(|| "connection closed".to_string()),
                        ))
                    }
                    RunEvent::Host(msg) => LoopEvent::Host(msg),
                    RunEvent::Job(_) | RunEvent::Retire => unreachable!("consumed above"),
                };
                note_turn_owner(&l.job);
                // The deadline as the heap knows it; a change during this
                // turn (settle → grace, a timer armed/fired/cleared)
                // re-queues below. Captured before the wait is folded.
                let queued_deadline = next_deadline(&l.rs);
                // The wait ended when the event ARRIVED; the delivery lag
                // was the loop's, not this run's.
                enter_turn(&mut l.rs, at);
                let outcome = {
                    let facts = RunFacts {
                        call: Some(&l.job.call),
                        reason: &core.reason,
                        guard: &core.guard,
                        cancel_handle: core.guard.handle(),
                    };
                    handle_loop_event(&mut core.isolate, &core.context, &facts, &mut l.rs, event)
                };
                let tainted_now = instance_tainted(core);
                match outcome {
                    Some(result) => {
                        conclude_run(&mut core.isolate, &core.run_table, l, result, tainted_now);
                    }
                    None => {
                        if tainted_now {
                            // Bare terminate mid-turn (a routed abort landed
                            // while this run's own turn ran): the owner is
                            // still live — classify and conclude it here.
                            let failure = l.rs.fail(reason_error(&core.reason));
                            conclude_run(&mut core.isolate, &core.run_table, l, Err(failure), true);
                        } else {
                            leave_turn(&mut l.rs);
                            let current = next_deadline(&l.rs);
                            if current != queued_deadline {
                                if let Some(at) = current {
                                    deadlines.push(std::cmp::Reverse((at, token)));
                                }
                            }
                            live.push(l);
                        }
                    }
                }
                drain_touched_timers(&live, &mut deadlines);
                if tainted_now {
                    taint_sweep(core, &mut live, events);
                    return;
                }
            }
        }

        load.close_turn();
        busy_nanos += turn_started.elapsed().as_nanos() as u64;
        let window = window_start.elapsed();
        if window.as_millis() as u64 >= LOAD_WINDOW_MS {
            let permille = (busy_nanos.saturating_mul(1000)
                / (window.as_nanos() as u64).max(1))
            .min(1000) as u32;
            load.close_window(permille);
            window_start = std::time::Instant::now();
            busy_nanos = 0;
        }
    }
}

/// Start one job on the instance. Returns whether the instance came out of
/// the dispatch tainted (mid-JS interruption during the start turn). A job
/// that suspends is pushed onto `live` with its boundary deadline (if any)
/// queued on `deadlines`.
fn dispatch_job(
    core: &mut InstanceCore,
    prefix_globals: &[types::HostGlobalDef],
    imports: &[types::ImportBinding],
    live: &mut Vec<LiveRun>,
    deadlines: &mut DeadlineHeap,
    mut job: Box<CallJob>,
) -> bool {
    if let Some(slot) = &job.ctl_slot {
        slot.set(core.guard.ctl()).ok();
    }
    note_turn_owner(&job);

    // Jobs interleave. The reason cell may only be reset while nothing else
    // is in flight; when runs are live it is guaranteed clear (a set reason
    // ends the loop).
    if live.is_empty() {
        core.reason.reset();
    }
    let token = if job.token != 0 {
        job.token
    } else {
        alloc_run_token()
    };
    let start = std::time::Instant::now();
    let cpu_budget = Arc::new(CpuBudget::new());
    let bridge_log: Arc<Mutex<BridgeCallLog>> = Arc::new(Mutex::new(BridgeCallLog::default()));

    // The run's table entry (see run_call_on_core — the same prelude).
    let entry = RunCallState::new(
        job.limits.max_stdout_bytes,
        job.limits.max_stderr_bytes,
        Arc::clone(&job.observer),
        Addr::Run(job.epilogue.map_or(0, |e| e.run_id)),
    );
    if core.warmup_logs_pending {
        core.warmup_logs_pending = false;
        let mut setup = core.run_table.setup_logs.borrow_mut();
        let mut logs = entry.logs.borrow_mut();
        logs.stdout = std::mem::take(&mut setup.stdout);
        logs.stderr = std::mem::take(&mut setup.stderr);
        logs.stdout_bytes = std::mem::take(&mut setup.stdout_bytes);
        logs.stderr_bytes = std::mem::take(&mut setup.stderr_bytes);
    }
    let logs_ptr: *const RefCell<LogBuffers> = std::ptr::addr_of!(*entry.logs);
    let pending_ptr: *const PendingWork = std::ptr::addr_of!(*entry.pending);
    let streams_ptr: *const StreamTable = std::ptr::addr_of!(*entry.streams);
    let out_streams_ptr: *const OutStreamTable = std::ptr::addr_of!(*entry.out_streams);
    let timers_ptr: *const RefCell<TimerTable> = std::ptr::addr_of!(*entry.timers);
    core.run_table.runs.borrow_mut().insert(token, entry);
    RUN_TABLE_PTR.with(|c| c.set(std::ptr::addr_of!(*core.run_table)));

    // Guard up before anything touches the retained context.
    core.guard.set_for(
        GuardTarget {
            budget: Arc::clone(&cpu_budget),
            cpu_cap_ms: job.limits.cpu_time_ms,
            wall_deadline: (job.limits.wall_time_ms > 0)
                .then(|| start + Duration::from_millis(job.limits.wall_time_ms as u64)),
        },
        token,
    );

    let placeholder_result = {
        v8::scope!(let scope, &mut core.isolate);
        let context = v8::Local::new(scope, &core.context);
        let scope = &mut v8::ContextScope::new(scope, context);
        install_prefix_bridge_placeholders(scope, prefix_globals, imports)
    };
    let cancel_handle = core.guard.handle().clone();
    let started = match placeholder_result {
        Err(error) => {
            core.guard.clear();
            let f = failure(
                termination_or(&core.reason, error),
                &unsafe { &*logs_ptr }.borrow(),
                start,
            );
            None.ok_or(f)
        }
        Ok(()) => {
            let io = std::mem::replace(&mut job.io, RunIo::None);
            let mut ctx = CallPhaseCtx {
                token,
                table: std::ptr::addr_of!(*core.run_table),
                limits: &job.limits,
                globals: &job.globals,
                imports,
                io,
                call_id_counter: Arc::clone(&job.call_id_counter),
                call: Some(&job.call),
                start,
                cpu_budget: &cpu_budget,
                bridge_log: &bridge_log,
                reason: &core.reason,
                logs: logs_ptr,
                pending: pending_ptr,
                streams: streams_ptr,
                out_streams: out_streams_ptr,
                timers: timers_ptr,
                epilogue: job.epilogue,
                cancel_handle: &cancel_handle,
            };
            let InstanceCore {
                isolate,
                context,
                prefix_module,
                stub_slots,
                guard,
                ..
            } = core;
            Ok(begin_call(
                isolate,
                context,
                RunEntry::Prefix(prefix_module),
                stub_slots,
                guard,
                &mut ctx,
            ))
        }
    };

    let tainted_now = instance_tainted(core);
    match started {
        Err(f) => {
            let l = LiveRun {
                rs: RunState {
                    token,
                    table: std::ptr::addr_of!(*core.run_table),
                    limits: job.limits,
                    start,
                    epilogue: job.epilogue,
                    source: None,
                    link: job.io.link(),
                    cpu_budget,
                    bridge_log,
                    bridge_error: Arc::new(OnceLock::new()),
                    resolvers: Arc::new(Mutex::new(HashMap::new())),
                    logs: logs_ptr,
                    pending: pending_ptr,
                    streams: streams_ptr,
                out_streams: out_streams_ptr,
                    timers: timers_ptr,
                    phase: RunPhase::Starting,
                    counted: Duration::ZERO,
                    parked_at: start,
                    turn_entered: Some(start),
                },
                job,
            };
            conclude_run(&mut core.isolate, &core.run_table, l, Err(f), tainted_now);
        }
        Ok((rs, outcome)) => {
            let l = LiveRun { rs, job };
            match outcome {
                ScanOutcome::Finished(result) => {
                    conclude_run(&mut core.isolate, &core.run_table, l, *result, tainted_now);
                }
                ScanOutcome::Continue => {
                    if tainted_now {
                        let f = l.rs.fail(reason_error(&core.reason));
                        conclude_run(&mut core.isolate, &core.run_table, l, Err(f), true);
                    } else {
                        // Covers a settling run's wall deadline, a run that
                        // entered grace within its start turn, and any
                        // timer the start turn armed.
                        if let Some(at) = next_deadline(&l.rs) {
                            deadlines.push(std::cmp::Reverse((at, l.rs.token)));
                        }
                        live.push(l);
                    }
                }
            }
        }
    }
    // A start turn can touch a co-resident's timers like any other turn.
    drain_touched_timers(live, deadlines);
    tainted_now
}

/// The taint path: every other in-flight run on the instance fails with
/// `ERR_INSTANCE_RESET` carrying the cause class and the culprit's wire run
/// id (E1 ruling 4), and queued jobs that never started are failed loudly.
/// The caller returns afterwards; the instance dies with its loop.
fn taint_sweep(
    core: &mut InstanceCore,
    live: &mut Vec<LiveRun>,
    events: &crossbeam_channel::Receiver<RoutedEvent>,
) {
    let cause = match core.reason.get() {
        Some(TerminationReason::Cpu) => ResetCause::Cpu,
        Some(TerminationReason::Memory) => ResetCause::Memory,
        Some(TerminationReason::Wall) => ResetCause::Wall,
        Some(TerminationReason::Import(_)) => ResetCause::Unsupported,
        Some(TerminationReason::Abort) | None => ResetCause::Abort,
    };
    // The culprit was concluded by the caller before the sweep; its wire id
    // travels via the guard's last executing turn... the loop tracks it as
    // the run it just processed. The sweep is only ever entered right after
    // that conclusion, so the culprit is simply the one run NOT in `live`.
    // Its wire id is recorded by the caller through `LAST_CULPRIT_RUN_ID`.
    let culprit_run_id = LAST_CULPRIT_RUN_ID.with(|c| c.get());
    for l in live.drain(..) {
        let f = l.rs.fail(RunError::InstanceReset {
            cause,
            culprit_run_id,
        });
        conclude_run(&mut core.isolate, &core.run_table, l, Err(f), true);
    }
    // Jobs already queued but never started: answer them instead of letting
    // their callers hang on a dropped channel. Queued events belong to runs
    // failed above (or concluded earlier) — dropped with the instance.
    while let Ok(routed) = events.try_recv() {
        let RunEvent::Job(mut job) = routed.event else {
            continue;
        };
        let outcome = CallOutcome {
            result: Err(failure(
                RunError::InstanceReset {
                    cause,
                    culprit_run_id,
                },
                &LogBuffers::default(),
                std::time::Instant::now(),
            )),
            tainted: true,
            heap_used_bytes: 0,
            prefix_failed: false,
        };
        let link = job.io.link();
        finish_job(&mut job, link, outcome);
    }
}

thread_local! {
    /// The wire run id of the turn owner most recently processed on this
    /// thread — the culprit named in an `ERR_INSTANCE_RESET` sweep.
    static LAST_CULPRIT_RUN_ID: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// A turn starts for `job`: record it as the sweep culprit and as the run
/// executing on this thread.
pub fn note_turn_owner(job: &CallJob) {
    let run_id = job.epilogue.map(|e| e.run_id).unwrap_or(0);
    LAST_CULPRIT_RUN_ID.with(|c| c.set(run_id));
    set_oom_run(OomRun::Run(run_id));
}

