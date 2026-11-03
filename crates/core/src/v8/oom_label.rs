//! The fatal OOM line: what this thread was executing when V8 aborts the
//! process, written to stderr from the OOM handler.

/// What this thread is executing for the fatal OOM line's benefit.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OomRun {
    None,
    /// A run's turn (wire run id).
    Run(u32),
    /// The prefix evaluating for the run that triggered this instance.
    ColdStart(u32),
}

/// Copy and destructor-free on purpose: read inside the OOM handler, where
/// the thread-local must neither allocate nor register anything.
#[derive(Clone, Copy)]
pub struct OomRunLabel {
    /// The instance thread's prefix; `None` on a one-off run's thread.
    pub prefix_id: Option<u64>,
    pub run: OomRun,
}

thread_local! {
    static OOM_RUN_LABEL: std::cell::Cell<OomRunLabel> = const {
        std::cell::Cell::new(OomRunLabel {
            prefix_id: None,
            run: OomRun::None,
        })
    };
}

/// Set once per instance thread, before the prefix evaluates.
pub fn set_oom_prefix_id(prefix_id: Option<u64>) {
    OOM_RUN_LABEL.with(|c| c.set(OomRunLabel { prefix_id, ..c.get() }));
}

/// Mark what this thread executes now; `OomRun::None` between runs.
pub fn set_oom_run(run: OomRun) {
    OOM_RUN_LABEL.with(|c| c.set(OomRunLabel { run, ..c.get() }));
}

/// The fatal OOM line, written into `buf` without allocating (the abort
/// path must not depend on a working allocator). Returns the bytes used.
fn format_oom_line(
    buf: &mut [u8],
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
    let label = OOM_RUN_LABEL.with(|c| c.get());
    let _ = match (label.run, label.prefix_id) {
        (OomRun::None, _) => write!(cur, " with no run executing on this thread"),
        (OomRun::Run(run_id), Some(p)) => write!(cur, " in run {run_id} of prefix {p}"),
        (OomRun::Run(run_id), None) => write!(cur, " in one-off run {run_id}"),
        (OomRun::ColdStart(run_id), Some(p)) => {
            write!(cur, " during cold start of prefix {p} (triggered by run {run_id})")
        }
        (OomRun::ColdStart(run_id), None) => {
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

/// Fatal OOM handler: V8 calls it on the isolate's thread right before it
/// aborts the process, so this line is the host's only attribution.
pub(super) unsafe extern "C" fn oom_error_cb(location: *const std::ffi::c_char, details: &v8::OomDetails) {
    use std::ffi::CStr;
    use std::io::Write as _;
    let mut buf = [0u8; 1024];
    let n = format_oom_line(
        &mut buf,
        details.is_heap_oom,
        (!details.detail.is_null()).then(|| unsafe { CStr::from_ptr(details.detail.cast()) }),
        (!location.is_null()).then(|| unsafe { CStr::from_ptr(location) }),
    );
    let _ = std::io::stderr().write_all(&buf[..n]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fatal_oom_line_names_the_run_executing_on_this_thread() {
        let line = |buf: &mut [u8]| {
            let n = format_oom_line(buf, true, Some(c"more"), Some(c"Reached heap limit"));
            String::from_utf8_lossy(&buf[..n]).into_owned()
        };
        let mut buf = [0u8; 256];
        set_oom_prefix_id(Some(123));
        set_oom_run(OomRun::Run(17));
        assert_eq!(
            line(&mut buf),
            "[iso4-v8] fatal: V8 out of memory (heap) in run 17 of prefix 123: \
             Reached heap limit (more) — aborting the process\n"
        );
        set_oom_run(OomRun::ColdStart(17));
        assert!(line(&mut buf).contains(" during cold start of prefix 123 (triggered by run 17): "));
        set_oom_prefix_id(None);
        set_oom_run(OomRun::Run(18));
        assert!(line(&mut buf).contains(" in one-off run 18: "));
        set_oom_run(OomRun::None);
        // Null detail and location are left out; a full buffer cuts, never grows.
        let n = format_oom_line(&mut buf, false, None, None);
        assert_eq!(
            &buf[..n],
            "[iso4-v8] fatal: V8 out of memory (non-heap) with no run executing on this thread \
             — aborting the process\n"
                .as_bytes()
        );
        let mut small = [0u8; 24];
        assert_eq!(format_oom_line(&mut small, true, None, None), 24);
        assert_eq!(&small, b"[iso4-v8] fatal: V8 out ");
    }
}
