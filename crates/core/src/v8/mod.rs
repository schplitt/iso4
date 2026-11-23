//! JavaScript execution: the engine core, one child module per concern.
//!
//! `runloop` holds the run entry points and the turn loop; `instance` the
//! owner thread serving a warm instance; `runtable` the per-run state table
//! and the CPED rider; `isolate` isolate creation; `oom_label` the fatal
//! hook; `exceptions` thrown-value handling; `console`, `timers`, `guard`,
//! `globals`, `modules`, `bridge` and `streams` the runtime surface those
//! names say; the `node:*` mirrors live under `node`. `RunError` and the
//! failure output live in `crate::errors`. This file only re-exports.

pub use crate::errors::{FailureOutput, RunError, RuntimeErrorData};
pub use crate::types::ResetCause;
pub use globals::PendingWork;
pub use guard::GuardCtl;
pub(crate) use instance::dead_instance_outcome;
pub use instance::{completion_message, finish_job, finish_run, send_completion, serve_instance};
pub use isolate::{
    CpuBudget, InstanceCore, Limits, PrefixSpec, create_instance_core, init_platform,
};
pub(crate) use oom_label::{
    fatal_hook, fatal_memory_error_cb, oom_error_cb, set_fatal_hook, v8_fatal_error_cb,
};
pub use oom_label::{FatalCause, FatalHook, FatalInfo, FatalPhase, OomRecord, OomRun, OomRunLabel};
pub use runloop::{
    CallJob, CallOutcome, EpilogueSpec, GraceReport, GraceStatus, Output, ReleaseHook, RoutedEvent,
    RunEvent, RunEventSender, RunIo, execute_with_io, set_run_epilogue_spec,
};
#[cfg(test)]
pub use runloop::{execute, execute_with_prefix, run_call_on_core};
pub use runtable::{InstanceLoad, RunTable, alloc_run_token, load_now_ms};
pub use streams::{
    OutStreamTable, StreamTable, is_stream_body, make_body_stream, register_hydrated_stream,
    register_out_stream,
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
