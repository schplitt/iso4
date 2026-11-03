//! JavaScript execution: the engine core, one child module per concern.
//!
//! `runloop` holds the run entry points and the turn loop; `instance` the
//! owner thread serving a warm instance; `runtable` the per-run state table
//! and the CPED rider; `isolate` isolate creation; `oom_label` the fatal OOM
//! line; `exceptions` thrown-value handling; `console`, `timers`, `guard`,
//! `globals`, `modules`, `bridge` and `streams` the runtime surface those
//! names say; the `node:*` mirrors live under `node`. `RunError` and the
//! failure output live in `crate::errors`. This file only re-exports.

pub use crate::errors::{FailureOutput, RunError, RuntimeErrorData};
pub use crate::types::ResetCause;
pub use globals::{PendingWork, RESERVED_GLOBAL_NAMES};
pub use guard::GuardCtl;
pub use instance::{
    completion_message, finish_job, finish_run, note_turn_owner, send_completion, serve_instance,
};
pub use isolate::{
    CpuBudget, InstanceCore, Limits, PrefixSpec, WARMUP_CPU_MS, WARMUP_WALL_MS,
    create_instance_core, init_platform, set_idle_settle_secs,
};
pub use oom_label::{OomRun, OomRunLabel, set_oom_prefix_id, set_oom_run};
pub use runloop::{
    CallJob, CallOutcome, EpilogueSpec, GraceReport, GraceStatus, Output, ReleaseHook, RoutedEvent,
    RunEvent, RunEventSender, RunIo, execute, execute_with_io, execute_with_prefix,
    run_call_on_core, set_run_epilogue_spec,
};
pub use runtable::{InstanceLoad, RunTable, alloc_run_token, load_now_ms};
pub use streams::{
    OutStreamTable, StreamTable, body_stream_id_of, is_stream_body, make_body_stream,
    register_hydrated_stream, register_out_stream,
};

mod bridge;
pub(crate) mod console;
mod exceptions;
pub(crate) mod globals;
mod guard;
mod instance;
pub(crate) mod isolate;
mod modules;
pub(crate) mod node;
mod oom_label;
pub(crate) mod runloop;
pub(crate) mod runtable;
pub(crate) mod streams;
#[cfg(test)]
pub(crate) mod testsupport;
pub(crate) mod timers;
