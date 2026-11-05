//! The fatal OOM line: what an isolate was executing when V8 aborts the
//! process, written to stderr from the OOM handlers.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// What an isolate is executing, for the fatal OOM line's benefit.
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

/// The fatal OOM line, written into `buf` without allocating (the abort
/// path must not depend on a working allocator). Returns the bytes used.
fn format_oom_line(
    buf: &mut [u8],
    label: Option<OomRunLabel>,
    is_heap_oom: bool,
    detail: Option<&std::ffi::CStr>,
    location: Option<&std::ffi::CStr>,
) -> usize {
    use std::io::Write as _;
    let cap = buf.len();
    let mut cur = buf;
    let _ = write!(
        cur,
        "[iso4-v8] fatal: V8 out of memory ({})",
        if is_heap_oom { "heap" } else { "non-heap" }
    );
    let _ = match label.map(|l| (l.run, l.prefix_id)) {
        None => write!(cur, " outside any isolate"),
        Some((OomRun::None, _)) => write!(cur, " with no run executing in this isolate"),
        Some((OomRun::Run(run_id), Some(p))) => write!(cur, " in run {run_id} of prefix {p}"),
        Some((OomRun::Run(run_id), None)) => write!(cur, " in one-off run {run_id}"),
        Some((OomRun::ColdStart(run_id), Some(p))) => {
            write!(cur, " during cold start of prefix {p} (triggered by run {run_id})")
        }
        Some((OomRun::ColdStart(run_id), None)) => {
            write!(cur, " during cold start (triggered by run {run_id})")
        }
    };
    // V8 passes its message ("Reached heap limit") as `location`.
    if let Some(l) = location.and_then(|l| l.to_str().ok()) {
        let _ = write!(cur, ": {l}");
    }
    if let Some(d) = detail.and_then(|d| d.to_str().ok()) {
        let _ = write!(cur, " ({d})");
    }
    let _ = write!(cur, " — aborting the process\n");
    cap - cur.len()
}

/// Set by the first OOM line: V8 calls the process-wide callback after an
/// isolate's handler too, and that second line would be wrong.
static OOM_LINE_WRITTEN: AtomicBool = AtomicBool::new(false);

fn write_oom_line(label: Option<OomRunLabel>, location: *const std::ffi::c_char, details: &v8::OomDetails) {
    use std::ffi::CStr;
    use std::io::Write as _;
    let mut buf = [0u8; 1024];
    let n = format_oom_line(
        &mut buf,
        label,
        details.is_heap_oom,
        (!details.detail.is_null()).then(|| unsafe { CStr::from_ptr(details.detail.cast()) }),
        (!location.is_null()).then(|| unsafe { CStr::from_ptr(location) }),
    );
    let _ = std::io::stderr().write_all(&buf[..n]);
}

/// The isolate's OOM handler, with its [`OomRecord`] as `data`. V8 calls it
/// right before it aborts the process, so this line is the host's only
/// attribution.
pub(crate) unsafe extern "C" fn oom_error_cb(
    location: *const std::ffi::c_char,
    details: &v8::OomDetails,
    data: *mut std::ffi::c_void,
) {
    let label = unsafe { data.cast::<OomRecord>().as_ref() }.map(OomRecord::label);
    OOM_LINE_WRITTEN.store(true, Ordering::Relaxed);
    write_oom_line(label, location, details);
}

/// The process-wide fallback for an OOM with no current isolate, such as a
/// zone allocation on a background compile thread.
pub(crate) unsafe extern "C" fn fatal_memory_error_cb(location: *const std::ffi::c_char, details: &v8::OomDetails) {
    if !OOM_LINE_WRITTEN.swap(true, Ordering::Relaxed) {
        write_oom_line(None, location, details);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fatal_oom_line_names_the_run_executing_in_the_isolate() {
        let line = |buf: &mut [u8], record: &OomRecord| {
            let n = format_oom_line(buf, Some(record.label()), true, Some(c"more"), Some(c"Reached heap limit"));
            String::from_utf8_lossy(&buf[..n]).into_owned()
        };
        let mut buf = [0u8; 256];
        let record = OomRecord::new(OomRunLabel {
            prefix_id: Some(123),
            run: OomRun::ColdStart(17),
        });
        assert!(line(&mut buf, &record).contains(" during cold start of prefix 123 (triggered by run 17): "));
        record.set_run(OomRun::Run(u32::MAX));
        assert_eq!(
            line(&mut buf, &record),
            "[iso4-v8] fatal: V8 out of memory (heap) in run 4294967295 of prefix 123: \
             Reached heap limit (more) — aborting the process\n"
        );
        record.set_run(OomRun::None);
        assert!(line(&mut buf, &record).contains(" with no run executing in this isolate: "));
        let one_off = OomRecord::new(OomRunLabel {
            prefix_id: None,
            run: OomRun::Run(18),
        });
        assert!(line(&mut buf, &one_off).contains(" in one-off run 18: "));
        // Null detail and location are left out; a full buffer cuts, never grows.
        let n = format_oom_line(&mut buf, None, false, None, None);
        assert_eq!(
            &buf[..n],
            "[iso4-v8] fatal: V8 out of memory (non-heap) outside any isolate — aborting the process\n".as_bytes()
        );
        let mut small = [0u8; 24];
        assert_eq!(format_oom_line(&mut small, None, true, None, None), 24);
        assert_eq!(&small, b"[iso4-v8] fatal: V8 out ");
    }
}
