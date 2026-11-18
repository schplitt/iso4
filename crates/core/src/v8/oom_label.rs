//! What an isolate was executing when V8 aborts the process, and the fatal
//! hook the embedder sets to report that death.

use std::sync::atomic::{AtomicPtr, AtomicU64, Ordering};

/// What an isolate is executing, for the fatal hook's benefit.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OomRun {
    None,
    /// A run's turn (wire run id).
    Run(u32),
    /// The prefix evaluating for the run that triggered this instance.
    ColdStart(u32),
}

#[derive(Clone, Copy)]
pub struct OomRunLabel {
    /// The instance's prefix; `None` on a one-off run's isolate.
    pub prefix_id: Option<u64>,
    pub run: OomRun,
}

impl OomRunLabel {
    pub const NONE: Self = Self {
        prefix_id: None,
        run: OomRun::None,
    };
}

/// An isolate's label, registered as its OOM handler's data. V8 may call the
/// handler on a GC worker thread, hence the atomic run word.
pub struct OomRecord {
    prefix_id: Option<u64>,
    /// `OomRun` as tag << 32 | run id.
    run: AtomicU64,
}

impl OomRecord {
    pub fn new(label: OomRunLabel) -> Box<Self> {
        let record = Box::new(Self {
            prefix_id: label.prefix_id,
            run: AtomicU64::new(0),
        });
        record.set_run(label.run);
        record
    }

    /// Mark what the isolate executes now; `OomRun::None` between runs.
    pub fn set_run(&self, run: OomRun) {
        let word = match run {
            OomRun::None => 0,
            OomRun::Run(id) => 1 << 32 | u64::from(id),
            OomRun::ColdStart(id) => 2 << 32 | u64::from(id),
        };
        self.run.store(word, Ordering::Relaxed);
    }

    fn label(&self) -> OomRunLabel {
        let word = self.run.load(Ordering::Relaxed);
        let run = match word >> 32 {
            1 => OomRun::Run(word as u32),
            2 => OomRun::ColdStart(word as u32),
            _ => OomRun::None,
        };
        OomRunLabel {
            prefix_id: self.prefix_id,
            run,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FatalCause {
    /// V8 ran out of memory in the JavaScript heap.
    OomHeap,
    /// V8 ran out of memory outside the heap, such as a zone allocation.
    OomOther,
    /// A V8 `CHECK` failure or other fatal error.
    V8Fatal,
}

/// What the dying isolate was executing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FatalPhase {
    /// A prefix run's turn.
    Run,
    /// A one-off run's turn.
    OneOff,
    /// A prefix evaluating for the run that triggered its instance.
    ColdStart,
    /// An isolate with no run executing.
    Idle,
    /// No current isolate.
    Outside,
}

/// One fatal V8 error, as the fatal hook receives it. `phase` is `None` for
/// [`FatalCause::V8Fatal`], which names no isolate.
#[derive(Clone, Copy, Debug)]
pub struct FatalInfo<'a> {
    pub cause: FatalCause,
    pub phase: Option<FatalPhase>,
    pub run: Option<u32>,
    pub prefix: Option<u64>,
    /// V8's message; empty when it gave none.
    pub message: &'a [u8],
    /// V8's detail on an out of memory; empty when it gave none.
    pub detail: &'a [u8],
}

/// Runs in a dying process, on any thread, and may run more than once for one
/// death. It must not allocate or take locks.
pub type FatalHook = fn(&FatalInfo<'_>);

static FATAL_HOOK: AtomicPtr<()> = AtomicPtr::new(std::ptr::null_mut());

/// Sets the hook V8's out of memory and fatal error callbacks report to. Set
/// it before anything starts V8 (`init_platform`): with no hook, core
/// registers none of those callbacks and V8 prints its own messages.
pub fn set_fatal_hook(hook: FatalHook) {
    FATAL_HOOK.store(hook as *mut (), Ordering::Release);
}

pub(crate) fn fatal_hook() -> Option<FatalHook> {
    let hook = FATAL_HOOK.load(Ordering::Acquire);
    // SAFETY: only `set_fatal_hook` stores into it, always a `FatalHook`.
    (!hook.is_null()).then(|| unsafe { std::mem::transmute::<*mut (), FatalHook>(hook) })
}

fn oom_info<'a>(
    record: Option<&OomRecord>,
    is_heap_oom: bool,
    message: &'a [u8],
    detail: &'a [u8],
) -> FatalInfo<'a> {
    let (phase, run, prefix) = match record.map(OomRecord::label) {
        None => (FatalPhase::Outside, None, None),
        Some(OomRunLabel {
            prefix_id,
            run: OomRun::None,
        }) => (FatalPhase::Idle, None, prefix_id),
        Some(OomRunLabel {
            prefix_id: None,
            run: OomRun::Run(id),
        }) => (FatalPhase::OneOff, Some(id), None),
        Some(OomRunLabel {
            prefix_id,
            run: OomRun::Run(id),
        }) => (FatalPhase::Run, Some(id), prefix_id),
        Some(OomRunLabel {
            prefix_id,
            run: OomRun::ColdStart(id),
        }) => (FatalPhase::ColdStart, Some(id), prefix_id),
    };
    FatalInfo {
        cause: if is_heap_oom {
            FatalCause::OomHeap
        } else {
            FatalCause::OomOther
        },
        phase: Some(phase),
        run,
        prefix,
        message,
        detail,
    }
}

fn c_bytes<'a>(s: *const std::ffi::c_char) -> &'a [u8] {
    if s.is_null() {
        return &[];
    }
    // SAFETY: V8 passes NUL-terminated strings that live through the call.
    unsafe { std::ffi::CStr::from_ptr(s) }.to_bytes()
}

fn report_oom(
    record: Option<&OomRecord>,
    location: *const std::ffi::c_char,
    details: &v8::OomDetails,
) {
    if let Some(hook) = fatal_hook() {
        // V8 passes its message ("Reached heap limit") as `location`.
        hook(&oom_info(
            record,
            details.is_heap_oom,
            c_bytes(location),
            c_bytes(details.detail.cast()),
        ));
    }
}

/// The isolate's OOM handler, with its [`OomRecord`] as `data`. V8 calls it
/// right before it aborts the process.
pub(crate) unsafe extern "C" fn oom_error_cb(
    location: *const std::ffi::c_char,
    details: &v8::OomDetails,
    data: *mut std::ffi::c_void,
) {
    report_oom(
        unsafe { data.cast::<OomRecord>().as_ref() },
        location,
        details,
    );
}

/// The process-wide OOM callback: V8 calls it on every fatal OOM, after the
/// isolate's handler if one ran.
pub(crate) unsafe extern "C" fn fatal_memory_error_cb(
    location: *const std::ffi::c_char,
    details: &v8::OomDetails,
) {
    report_oom(None, location, details);
}

/// V8's process-wide fatal error callback. File and line are empty at this
/// build's logging level.
pub(crate) fn v8_fatal_error_cb(_file: &str, _line: i32, message: &str) {
    if let Some(hook) = fatal_hook() {
        hook(&FatalInfo {
            cause: FatalCause::V8Fatal,
            phase: None,
            run: None,
            prefix: None,
            message: message.as_bytes(),
            detail: &[],
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_record_maps_to_the_phase_run_and_prefix() {
        let at = |record: Option<&OomRecord>| {
            let info = oom_info(record, true, b"m", b"");
            (info.phase.unwrap(), info.run, info.prefix)
        };
        let record = OomRecord::new(OomRunLabel {
            prefix_id: Some(123),
            run: OomRun::ColdStart(17),
        });
        assert_eq!(
            at(Some(&record)),
            (FatalPhase::ColdStart, Some(17), Some(123))
        );
        record.set_run(OomRun::Run(u32::MAX));
        assert_eq!(
            at(Some(&record)),
            (FatalPhase::Run, Some(u32::MAX), Some(123))
        );
        record.set_run(OomRun::None);
        assert_eq!(at(Some(&record)), (FatalPhase::Idle, None, Some(123)));
        let one_off = OomRecord::new(OomRunLabel {
            prefix_id: None,
            run: OomRun::Run(18),
        });
        assert_eq!(at(Some(&one_off)), (FatalPhase::OneOff, Some(18), None));
        assert_eq!(at(None), (FatalPhase::Outside, None, None));
        assert_eq!(oom_info(None, false, b"", b"").cause, FatalCause::OomOther);
    }
}
