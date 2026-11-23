//! JavaScript execution: the engine core, one child module per concern.
//!
//! `runloop` holds the run entry points and the turn loop; `instance` the
//! owner thread serving a warm instance; `runtable` the per-run state table
//! and the CPED rider; `isolate` isolate creation; `oom_label` the fatal
//! hook; `exceptions` thrown-value handling; `console`, `timers`, `guard`,
//! `globals`, `modules`, `bridge` and `streams` the runtime surface those
//! names say; the `node:*` mirrors live under `node`. `RunError` and the
//! failure output live in `crate::errors`. This file only re-exports;
//! code outside `v8/` imports from `crate::v8` only, the submodules are private.

pub use crate::errors::{FailureOutput, RunError, RuntimeErrorData};
pub use crate::types::ResetCause;
pub(crate) use console::{console_stderr_callback, console_stdout_callback, install_console};
pub use globals::PendingWork;
pub(crate) use globals::{install_wait_until, install_web_runtime, wait_until_callback};
pub use guard::GuardCtl;
pub(crate) use instance::dead_instance_outcome;
pub use instance::{completion_message, finish_job, finish_run, send_completion, serve_instance};
pub use isolate::{
    CpuBudget, InstanceCore, Limits, PrefixSpec, create_instance_core, init_platform,
};
pub(crate) use isolate::{
    LogBuffers, ReasonCell, TerminationReason, WARMUP_CPU_MS, WARMUP_WALL_MS, cores_option,
    elapsed_ms, init_platform_with, set_cores, set_idle_settle_secs,
};
#[cfg(test)]
pub(crate) use isolate::idle_settle_after;
pub(crate) use node::async_hooks::{
    async_context_get_callback, async_context_set_callback, install_async_context,
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
pub(crate) use runtable::RunTableSlot;
pub use streams::{
    OutStreamTable, StreamTable, is_stream_body, make_body_stream, register_hydrated_stream,
    register_out_stream,
};
#[cfg(test)]
pub(crate) use streams::OUT_IDLE_OVERRIDE_MS;
pub(crate) use streams::{stamp_stream_body_callback, stream_cancel_callback, stream_read_callback};
pub(crate) use timers::{clear_timeout_callback, install_timers, set_timeout_callback};

mod bridge;
mod console;
mod exceptions;
mod globals;
mod guard;
mod instance;
mod isolate;
mod modules;
mod node;
mod oom_label;
mod runloop;
mod runtable;
mod streams;
#[cfg(test)]
mod testsupport;
mod timers;
