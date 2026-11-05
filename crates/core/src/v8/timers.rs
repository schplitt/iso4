//! Guest timers: `setTimeout`/`clearTimeout` over the attributed run's
//! [`TimerTable`], and the turn that fires one.

use std::cell::RefCell;
use std::collections::{BinaryHeap, HashMap};
use std::time::Duration;

use crate::v8::RunError;
use crate::v8::exceptions::runtime_error_from_value;
use crate::v8::globals::{define_global, throw_v8_error};
use crate::v8::runloop::{
    BudgetEpoch, RunFacts, RunPhase, RunState, ScanOutcome, TurnGuard, capped_grace_error,
    finish_post_phase, finish_turn, finished,
};
use crate::v8::runtable::{
    CURRENT_RUN_TOKEN, RUN_TABLE_PTR, TIMERS_TOUCHED, attributed_token_and_state,
    install_cped_rider, slot_table,
};

/// Cap on pending timers per run (workerd's number). The entries live
/// outside the isolate heap, where `memoryMb` cannot see them.
const MAX_PENDING_TIMERS: usize = 10_000;

/// Delay cap (workerd's "100 years") — bounds the `Instant` arithmetic.
const MAX_TIMER_DELAY_MS: f64 = 3_153_600_000_000.0;

/// One armed `setTimeout` registration.
struct TimerEntry {
    callback: v8::Global<v8::Function>,
    /// Extra `setTimeout(fn, ms, ...args)` arguments.
    args: Vec<v8::Global<v8::Value>>,
    /// Real instant the loop waits on: when the wall reaches `virtual_ms`.
    deadline: std::time::Instant,
    /// What the clock advances to at fire (frozen-at-registration + delay):
    /// the callback sees the scheduled time, never real wall (S0 freeze).
    virtual_ms: f64,
}

/// A run's pending timers: id map + fire-order heap (deadline, then `seq`
/// = creation order). Lazily deleted — `clearTimeout` removes only the map
/// entry.
#[derive(Default)]
pub struct TimerTable {
    /// Ids start at 1; wrapping needs 2^32 registrations in one run.
    last_id: u32,
    next_seq: u64,
    entries: HashMap<u32, TimerEntry>,
    order: BinaryHeap<std::cmp::Reverse<(std::time::Instant, u64, u32)>>,
}

impl TimerTable {
    pub(super) fn boxed() -> Box<RefCell<Self>> {
        Box::new(RefCell::new(Self::default()))
    }

    /// The earliest pending deadline, discarding cleared heap tops (the
    /// deadline match also guards id reuse).
    pub(super) fn peek_deadline(&mut self) -> Option<std::time::Instant> {
        loop {
            let std::cmp::Reverse((at, _, id)) = *self.order.peek()?;
            if self.entries.get(&id).is_some_and(|e| e.deadline == at) {
                return Some(at);
            }
            self.order.pop();
        }
    }

    /// Remove and return the earliest pending timer (a fire).
    fn pop_earliest(&mut self) -> Option<TimerEntry> {
        loop {
            let std::cmp::Reverse((at, _, id)) = self.order.pop()?;
            if self.entries.get(&id).is_some_and(|e| e.deadline == at) {
                return self.entries.remove(&id);
            }
        }
    }
}

/// One timer-driven turn: fire the run's earliest pending timer,
/// checkpoint, scan. The clock advances to the timer's SCHEDULED time,
/// never real wall (a timer must not reveal real elapsed time — S0).
pub(super) fn timer_turn(
    isolate: &mut v8::OwnedIsolate,
    context: &v8::Global<v8::Context>,
    facts: &RunFacts<'_>,
    rs: &mut RunState,
) -> ScanOutcome {
    // SAFETY: `timers` points into the run's live table entry.
    let Some(entry) = unsafe { &*rs.timers }.borrow_mut().pop_earliest() else {
        return ScanOutcome::Continue; // cleared in the meantime
    };
    let in_grace = matches!(rs.phase, RunPhase::Grace(_));

    // The callback is guest JS billed to the run that armed the timer.
    let _turn = TurnGuard::begin(facts.guard, rs);
    let _epoch = BudgetEpoch::enter(rs.active_budget());

    v8::scope!(let scope, isolate);
    let context_local = v8::Local::new(scope, context);
    let scope = &mut v8::ContextScope::new(scope, context_local);

    crate::webtypes::advance_frozen_clock_to(scope, entry.virtual_ms);
    CURRENT_RUN_TOKEN.with(|c| c.set(rs.token));
    RUN_TABLE_PTR.with(|c| c.set(rs.table));
    install_cped_rider(scope, rs.token);

    v8::tc_scope!(let scope, scope);
    let callback = v8::Local::new(scope, &entry.callback);
    let recv = v8::undefined(scope).into();
    let call_args: Vec<v8::Local<v8::Value>> = entry
        .args
        .iter()
        .map(|a| v8::Local::new(scope, a))
        .collect();
    if callback.call(scope, recv, &call_args).is_none() {
        // A kill leaves reason/termination for the shared tail; a plain
        // uncaught throw fails the run.
        if facts.reason.get().is_none() && !scope.is_execution_terminating() {
            let err = match scope.exception() {
                Some(exception) => runtime_error_from_value(scope, exception),
                None => {
                    RunError::Internal("timer callback failed without an exception".to_string())
                }
            };
            if facts.reason.get().is_some() {
                return finish_turn(scope, facts, rs, in_grace);
            }
            if in_grace {
                let (name, message) = match &err {
                    RunError::RuntimeError(d) => (d.name.clone(), d.message.clone()),
                    other => ("Error".to_string(), format!("{other:?}")),
                };
                if let RunPhase::Grace(g) = &mut rs.phase {
                    if g.error.is_none() {
                        g.error = Some(capped_grace_error(name, message));
                    }
                }
                return finished(Ok(finish_post_phase(rs)));
            }
            return finished(Err(rs.fail(err)));
        }
    }

    scope.perform_microtask_checkpoint();

    finish_turn(scope, facts, rs, in_grace)
}

/// Install `setTimeout`/`clearTimeout`: natives over the attributed run's
/// [`TimerTable`] (the `waitUntil` pattern). Outside run code `setTimeout`
/// throws a catchable error (workerd's explicit route).
pub(crate) fn install_timers(scope: &mut v8::PinScope) -> Result<(), RunError> {
    for (name, builder) in [
        ("setTimeout", v8::Function::builder(set_timeout_callback)),
        (
            "clearTimeout",
            v8::Function::builder(clear_timeout_callback),
        ),
    ] {
        let function = builder
            .build(scope)
            .ok_or_else(|| RunError::Internal(format!("failed to create {name}")))?;
        define_global(scope, name, function.into(), false)?;
    }
    Ok(())
}

/// `setTimeout(callback, delay, ...args)` — arm a timer on the attributed
/// run; returns a per-run numeric id. Scheduled from the frozen clock, so
/// the guest-visible story is exactly "delay ms passed" (workerd model).
pub(crate) fn set_timeout_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let (token, state) = slot_table(scope)
        .map_or((0, std::ptr::null()), |table| attributed_token_and_state(scope, table));
    if state.is_null() {
        throw_v8_error(
            scope,
            "[iso4] setTimeout is only available to run code — setup evaluated at \
             prepare() time must finish its work within the warm-up budget",
        );
        return;
    }
    let Ok(callback) = v8::Local::<v8::Function>::try_from(args.get(0)) else {
        let message = v8::String::new(
            scope,
            "[iso4] setTimeout requires a function callback (string callbacks are not \
             supported)",
        )
        .unwrap_or_else(|| v8::String::empty(scope));
        let exception = v8::Exception::type_error(scope, message);
        scope.throw_exception(exception);
        return;
    };
    // NaN/negative → 0, capped at 100 years, whole ms.
    let mut delay = args
        .get(1)
        .number_value(scope)
        .filter(|d| d.is_finite())
        .unwrap_or(0.0);
    delay = delay.trunc().clamp(0.0, MAX_TIMER_DELAY_MS);
    let extra: Vec<v8::Global<v8::Value>> = (2..args.length())
        .map(|i| v8::Global::new(scope, args.get(i)))
        .collect();

    // Fires at the wall instant where the clock would reach `virtual_ms`;
    // `lead` shrinks by the clock's lag and floors at zero (due now).
    let wall = crate::webtypes::wall_now_ms();
    let frozen = crate::webtypes::frozen_now_ms(scope).unwrap_or(wall);
    let virtual_ms = frozen + delay;
    let lead = (virtual_ms - wall).max(0.0);
    let deadline = std::time::Instant::now() + Duration::from_millis(lead as u64);

    // SAFETY: entry pointers are valid for the remainder of the turn.
    let mut timers = unsafe { &*state }.timers.borrow_mut();
    if timers.entries.len() >= MAX_PENDING_TIMERS {
        drop(timers);
        throw_v8_error(
            scope,
            "[iso4] too many active timers: at most 10000 may be pending per run",
        );
        return;
    }
    timers.last_id = timers.last_id.wrapping_add(1).max(1);
    let id = timers.last_id;
    let seq = timers.next_seq;
    timers.next_seq += 1;
    timers.entries.insert(
        id,
        TimerEntry {
            callback: v8::Global::new(scope, callback),
            args: extra,
            deadline,
            virtual_ms,
        },
    );
    timers.order.push(std::cmp::Reverse((deadline, seq, id)));
    drop(timers);
    TIMERS_TOUCHED.with(|v| v.borrow_mut().push(token));
    rv.set(v8::Number::new(scope, id as f64).into());
}

/// `clearTimeout(id)` — disarm one of the attributed run's own timers.
/// Everything else (unknown/foreign ids, garbage, outside run code) is a
/// silent no-op, web semantics.
pub(crate) fn clear_timeout_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    rv.set_undefined();
    let (token, state) = slot_table(scope)
        .map_or((0, std::ptr::null()), |table| attributed_token_and_state(scope, table));
    if state.is_null() {
        return;
    }
    let Some(id) = args
        .get(0)
        .number_value(scope)
        .filter(|n| n.is_finite() && *n >= 1.0 && *n <= u32::MAX as f64 && n.fract() == 0.0)
    else {
        return;
    };
    // SAFETY: entry pointers are valid for the remainder of the turn.
    let removed = unsafe { &*state }
        .timers
        .borrow_mut()
        .entries
        .remove(&(id as u32))
        .is_some();
    if removed {
        // The next deadline may have moved later; let the loop re-queue.
        TIMERS_TOUCHED.with(|v| v.borrow_mut().push(token));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicU32;

    use super::*;
    use crate::v8::testsupport::{
        call_spec, get_default, grace_limits, run_code, run_err, run_ok, value_of,
    };
    use crate::testval::TestValue as WireValue;
    use crate::v8::{
        GraceStatus, Limits, PrefixSpec, RunError, RunIo, create_instance_core, execute,
        init_platform, run_call_on_core,
    };

    // ── Guest timers ────────────────────────────────────────────────────────

    #[test]
    fn sleep_resolves_without_a_socket() {
        // The reported idiom, in a run with no bridge at all: the drive loop
        // parks the thread to the timer deadline instead of failing the
        // pending root.
        let out = run_ok("await new Promise(r => setTimeout(r, 30))\nexport default 'woke'");
        assert_eq!(get_default(&out).as_deref(), Some("woke"));
    }

    #[test]
    fn sleep_costs_no_cpu_budget() {
        // A 100 ms sleep under a 50 ms CPU cap: waiting is not computing.
        let out = run_code(
            "await new Promise(r => setTimeout(r, 100))\nexport default 'ok'",
            "<iso4>",
            Limits {
                cpu_time_ms: 50,
                ..Limits::default()
            },
        )
        .expect("sleep does not bill CPU");
        assert_eq!(get_default(&out).as_deref(), Some("ok"));
        assert!(
            out.cpu_time_ms < 50.0,
            "cpu_time_ms {} should exclude the sleep",
            out.cpu_time_ms
        );
    }

    #[test]
    fn sleep_counts_against_the_wall_budget() {
        let err = run_code(
            "await new Promise(r => setTimeout(r, 5000))\nexport default 'nope'",
            "<iso4>",
            Limits {
                wall_time_ms: 80,
                ..Limits::default()
            },
        )
        .expect_err("an over-long sleep dies at the wall");
        assert!(matches!(err.error, RunError::WallTimeout));
    }

    #[test]
    fn timer_due_before_the_wall_fires_first() {
        // Timer at 30 ms, wall at 300 ms: the timer must win, not be
        // swallowed by a premature wall conclusion.
        let out = run_code(
            "await new Promise(r => setTimeout(r, 30))\nexport default 'made it'",
            "<iso4>",
            Limits {
                wall_time_ms: 300,
                ..Limits::default()
            },
        )
        .expect("a timer inside the wall budget fires");
        assert_eq!(get_default(&out).as_deref(), Some("made it"));
    }

    #[test]
    fn clear_timeout_prevents_the_callback() {
        let out = run_ok(
            "let fired = false\n\
             const id = setTimeout(() => { fired = true }, 10)\n\
             clearTimeout(id)\n\
             await new Promise(r => setTimeout(r, 60))\n\
             export default String(fired)",
        );
        assert_eq!(get_default(&out).as_deref(), Some("false"));
    }

    #[test]
    fn clear_timeout_ignores_unknown_and_garbage_ids() {
        let out = run_ok(
            "clearTimeout(9999)\nclearTimeout(undefined)\nclearTimeout('x')\n\
             clearTimeout(-1)\nclearTimeout(1.5)\nexport default 'ok'",
        );
        assert_eq!(get_default(&out).as_deref(), Some("ok"));
    }

    #[test]
    fn timers_fire_in_deadline_order_then_creation_order() {
        let out = run_ok(
            "const seen = []\n\
             setTimeout(() => seen.push('late'), 50)\n\
             setTimeout(() => seen.push('early'), 10)\n\
             setTimeout(() => seen.push('tie1'), 25)\n\
             setTimeout(() => seen.push('tie2'), 25)\n\
             await new Promise(r => setTimeout(r, 80))\n\
             export default seen.join(',')",
        );
        assert_eq!(
            get_default(&out).as_deref(),
            Some("early,tie1,tie2,late"),
            "deadline order, creation order on equal deadlines"
        );
    }

    #[test]
    fn set_timeout_returns_numeric_ids_from_one() {
        let out = run_ok(
            "const a = setTimeout(() => {}, 1000)\n\
             const b = setTimeout(() => {}, 1000)\n\
             export default [typeof a, a, typeof b, b].join('|')",
        );
        assert_eq!(get_default(&out).as_deref(), Some("number|1|number|2"));
    }

    #[test]
    fn set_timeout_passes_extra_arguments() {
        let out = run_ok(
            "const got = await new Promise(r => setTimeout((x, y) => r(x + y), 5, 'a', 'b'))\n\
             export default got",
        );
        assert_eq!(get_default(&out).as_deref(), Some("ab"));
    }

    #[test]
    fn set_timeout_rejects_non_function_callbacks() {
        // No string-eval form (codegen is denied per-run anyway): a
        // TypeError, catchable.
        let out = run_ok(
            "let caught = 'none'\n\
             try { setTimeout('globalThis.x = 1', 5) } catch (e) { caught = e.constructor.name }\n\
             let caught2 = 'none'\n\
             try { setTimeout(undefined) } catch (e) { caught2 = e.constructor.name }\n\
             export default [caught, caught2].join('|')",
        );
        assert_eq!(get_default(&out).as_deref(), Some("TypeError|TypeError"));
    }

    #[test]
    fn nan_and_negative_delays_fire_immediately() {
        let out = run_ok(
            "const a = await new Promise(r => setTimeout(() => r('nan'), NaN))\n\
             const b = await new Promise(r => setTimeout(() => r('neg'), -50))\n\
             export default [a, b].join('|')",
        );
        assert_eq!(get_default(&out).as_deref(), Some("nan|neg"));
    }

    #[test]
    fn pending_timer_cap_throws_past_ten_thousand() {
        let out = run_ok(
            "const cb = () => {}\n\
             for (let i = 0; i < 10000; i++) setTimeout(cb, 60000)\n\
             let caught = 'none'\n\
             try { setTimeout(cb, 60000) } catch (e) { caught = e.message }\n\
             clearTimeout(1) // free a slot: the cap is on PENDING timers\n\
             const again = typeof setTimeout(cb, 60000)\n\
             export default [caught.includes('10000'), again].join('|')",
        );
        assert_eq!(get_default(&out).as_deref(), Some("true|number"));
    }

    #[test]
    fn uncaught_throw_in_a_timer_callback_fails_the_run() {
        let err = run_err(
            "setTimeout(() => { throw new Error('boom in timer') }, 10)\n\
             await new Promise(r => setTimeout(r, 60))\n\
             export default 'unreachable'",
        );
        match err {
            RunError::RuntimeError(data) => {
                assert!(data.message.contains("boom in timer"), "{}", data.message)
            }
            other => panic!("expected RuntimeError, got {other:?}"),
        }
    }

    #[test]
    fn timer_callback_sees_exactly_the_scheduled_time() {
        // The workerd model, pinned: the clock advances by the REQUESTED
        // delay, so the delta is exact regardless of overshoot.
        let out = run_ok(
            "const t0 = Date.now()\n\
             await new Promise(r => setTimeout(r, 120))\n\
             export default String(Date.now() - t0)",
        );
        assert_eq!(get_default(&out).as_deref(), Some("120"));
    }

    #[test]
    fn zero_delay_timer_after_cpu_burn_reveals_no_elapsed_time() {
        // The point of virtualizing: a setTimeout(0) probe after a CPU
        // burn must not read real elapsed wall time back (S0 freeze).
        let out = run_ok(
            "const t0 = Date.now()\n\
             let acc = 0\n\
             for (let i = 0; i < 5_000_000; i++) acc = (acc + i) % 97\n\
             await new Promise(r => setTimeout(r, 0))\n\
             export default String(Date.now() - t0)",
        );
        assert_eq!(get_default(&out).as_deref(), Some("0"));
    }

    #[test]
    fn timers_work_during_grace_and_grace_ms_caps_them() {
        // A short sleep inside waitUntil completes within grace...
        let out = execute(
            "waitUntil((async () => { await new Promise(r => setTimeout(r, 20)); console.log('bg woke') })())\n\
             export default 'early'",
            None,
            grace_limits(),
            &[],
            &[],
            RunIo::None,
            Arc::new(AtomicU32::new(0)),
            None,
        )
        .unwrap();
        assert_eq!(get_default(&out).as_deref(), Some("early"));
        let bg = out.background.expect("grace phase ran");
        assert_eq!(bg.status, GraceStatus::Settled);
        assert_eq!(bg.stdout, vec!["bg woke".to_string()]);

        // ...and one longer than graceMs is truncated with the phase.
        let out = execute(
            "waitUntil(new Promise(r => setTimeout(r, 60000)))\nexport default 'early'",
            None,
            Limits {
                grace_ms: 60,
                ..Limits::default()
            },
            &[],
            &[],
            RunIo::None,
            Arc::new(AtomicU32::new(0)),
            None,
        )
        .unwrap();
        assert_eq!(get_default(&out).as_deref(), Some("early"));
        assert_eq!(
            out.background.expect("grace phase ran").status,
            GraceStatus::Truncated
        );
    }

    #[test]
    fn a_settled_runs_pending_timer_never_fires_into_the_next_run() {
        // Run 1 settles with a timer pending; the heap drops with its
        // table entry, so the callback can never fire into run 2.
        init_platform();
        let mut core = create_instance_core(
            PrefixSpec {
                code: "globalThis.hit = 'untouched'\n\
                       export function arm() { setTimeout(() => { globalThis.hit = 'FIRED' }, 20); return 'armed' }\n\
                       export async function read() { await new Promise(r => setTimeout(r, 30)); return globalThis.hit }",
                filename: "<prefix>",
                globals: &[],
            },
            &[],
            0,
            crate::v8::OomRunLabel::NONE,
        )
        .expect("prefix warms up");
        let run1 = run_call_on_core(
            &mut core,
            &[],
            Limits::default(),
            &[],
            &[],
            RunIo::None,
            Arc::new(AtomicU32::new(0)),
            &call_spec("arm", &[]),
        );
        assert!(!run1.tainted);
        assert_eq!(
            value_of(&run1.result.expect("run 1 succeeds")),
            WireValue::String("armed".to_string())
        );
        std::thread::sleep(Duration::from_millis(60));
        let run2 = run_call_on_core(
            &mut core,
            &[],
            Limits::default(),
            &[],
            &[],
            RunIo::None,
            Arc::new(AtomicU32::new(0)),
            &call_spec("read", &[]),
        );
        assert_eq!(
            value_of(&run2.result.expect("run 2 succeeds")),
            WireValue::String("untouched".to_string()),
            "run 1's pending timer must not fire on run 2's turns"
        );
    }

    #[test]
    fn set_timeout_at_prefix_top_level_throws_a_catchable_setup_error() {
        // workerd's explicit route for the analogous case: no loop exists
        // while a prefix evaluates, so the error is thrown, not a hang.
        init_platform();
        let mut core = create_instance_core(
            PrefixSpec {
                code: "let msg = 'no error'\n\
                       try { setTimeout(() => {}, 5) } catch (e) { msg = e.message }\n\
                       globalThis.prefixTimerError = msg\n\
                       export default () => globalThis.prefixTimerError",
                filename: "<prefix>",
                globals: &[],
            },
            &[],
            0,
            crate::v8::OomRunLabel::NONE,
        )
        .expect("prefix warms up despite the refused setTimeout");
        let out = run_call_on_core(
            &mut core,
            &[],
            Limits::default(),
            &[],
            &[],
            RunIo::None,
            Arc::new(AtomicU32::new(0)),
            &call_spec("default", &[]),
        )
        .result
        .expect("run succeeds");
        let WireValue::String(msg) = value_of(&out) else {
            panic!("expected a string value");
        };
        assert!(
            msg.contains("only available to run code"),
            "prefix-time setTimeout must throw the setup-time error, got: {msg}"
        );
    }

    #[test]
    fn a_prefix_defined_sleep_helper_works_during_a_run() {
        init_platform();
        let mut core = create_instance_core(
            PrefixSpec {
                code: "globalThis.sleep = (ms) => new Promise(r => setTimeout(r, ms))\n\
                       export default async () => { await sleep(20); return 'slept' }",
                filename: "<prefix>",
                globals: &[],
            },
            &[],
            0,
            crate::v8::OomRunLabel::NONE,
        )
        .expect("prefix warms up");
        let out = run_call_on_core(
            &mut core,
            &[],
            Limits::default(),
            &[],
            &[],
            RunIo::None,
            Arc::new(AtomicU32::new(0)),
            &call_spec("default", &[]),
        )
        .result
        .expect("run succeeds");
        assert_eq!(value_of(&out), WireValue::String("slept".to_string()));
    }
}
