//! The `crash` file next to the socket and the one fatal line a dying child
//! writes to it and to stderr: core's fatal hook and the signal handlers.

use std::ffi::{c_int, c_void};
use std::fmt::Write as _;
use std::os::fd::IntoRawFd as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;
use std::sync::atomic::{AtomicI32, AtomicU8, Ordering};
use std::sync::OnceLock;

use iso4_core::{FatalCause, FatalInfo, FatalPhase};

/// The line's bound, newline included.
const LINE_MAX: usize = 1024;

static CRASH_FD: AtomicI32 = AtomicI32::new(-1);

const FREE: u8 = 0;
const WRITING: u8 = 1;
const DONE: u8 = 2;
/// One line per death: one death can pass several writers, on several threads.
static STATE: AtomicU8 = AtomicU8::new(FREE);

/// SIGSEGV and SIGBUS first: only they keep the handler before ours.
const SIGNALS: [(c_int, &str); 5] = [
    (libc::SIGSEGV, "SIGSEGV"),
    (libc::SIGBUS, "SIGBUS"),
    (libc::SIGTRAP, "SIGTRAP"),
    (libc::SIGABRT, "SIGABRT"),
    (libc::SIGILL, "SIGILL"),
];

/// The child's `--key`, set once before the handlers; read without allocating.
static KEY: OnceLock<String> = OnceLock::new();

/// Rust std's handler, which reports a stack overflow and otherwise returns.
static PREVIOUS: [OnceLock<libc::sigaction>; 2] = [const { OnceLock::new() }; 2];

/// Opens the crash file and installs the signal handlers. Call before V8
/// starts; [`on_fatal`] goes to `EngineOptions::on_fatal`.
pub fn install(socket_path: &str, key: Option<&str>) {
    if let Some(key) = key {
        let _ = KEY.set(key.to_string());
    }
    let dir = Path::new(socket_path).parent().unwrap_or(Path::new(""));
    let path = dir.join("crash");
    match std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .custom_flags(libc::O_APPEND)
        .open(&path)
    {
        Ok(file) => CRASH_FD.store(file.into_raw_fd(), Ordering::Relaxed),
        Err(e) => iso4_core::log!(
            "[iso4-v8] cannot open the crash file at {}, a fatal line goes to stderr only: {e}",
            path.display()
        ),
    }
    for (i, &(sig, _)) in SIGNALS.iter().enumerate() {
        // SAFETY: all zeros is a valid sigaction; the fields are set below.
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        let mut previous: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction =
            on_signal as extern "C" fn(c_int, *mut libc::siginfo_t, *mut c_void) as usize;
        action.sa_flags = libc::SA_SIGINFO | libc::SA_ONSTACK;
        // SAFETY: both structs are valid for the call.
        unsafe {
            // No listed signal nests on the writing thread: it would wait for itself.
            libc::sigemptyset(&raw mut action.sa_mask);
            for &(other, _) in &SIGNALS {
                libc::sigaddset(&raw mut action.sa_mask, other);
            }
            libc::sigaction(sig, &raw const action, &raw mut previous);
        }
        if let Some(slot) = PREVIOUS.get(i) {
            let _ = slot.set(previous);
        }
    }
}

pub(crate) fn on_fatal(info: &FatalInfo<'_>) {
    let cause = match info.cause {
        FatalCause::OomHeap => "oom-heap",
        FatalCause::OomOther => "oom-other",
        FatalCause::V8Fatal => "v8-fatal",
    };
    report(cause, Some(info));
}

extern "C" fn on_signal(sig: c_int, info: *mut libc::siginfo_t, context: *mut c_void) {
    #[cfg(target_os = "macos")]
    // SAFETY: the calling thread's errno.
    let errno = unsafe { libc::__error() };
    #[cfg(not(target_os = "macos"))]
    // SAFETY: the calling thread's errno.
    let errno = unsafe { libc::__errno_location() };
    // SAFETY: `errno` points at this thread's errno.
    let saved = unsafe { *errno };
    let index = SIGNALS.iter().position(|&(s, _)| s == sig);
    let name = index
        .and_then(|i| SIGNALS.get(i))
        .map_or("signal", |&(_, name)| name);
    report(name, None);
    if let Some(previous) = index.and_then(|i| PREVIOUS.get(i)).and_then(OnceLock::get) {
        let handler = previous.sa_sigaction;
        if handler != libc::SIG_DFL && handler != libc::SIG_IGN {
            // SAFETY: the kernel accepted this address as the handler of that shape.
            unsafe {
                if previous.sa_flags & libc::SA_SIGINFO != 0 {
                    let f = std::mem::transmute::<
                        usize,
                        extern "C" fn(c_int, *mut libc::siginfo_t, *mut c_void),
                    >(handler);
                    f(sig, info, context);
                } else {
                    std::mem::transmute::<usize, extern "C" fn(c_int)>(handler)(sig);
                }
            }
        }
    }
    // Raised under the default action, the same signal ends the process once
    // this handler returns; returning alone may fault into another signal.
    // SAFETY: all zeros is SIG_DFL with an empty mask.
    let default: libc::sigaction = unsafe { std::mem::zeroed() };
    // SAFETY: async-signal-safe calls with valid arguments.
    unsafe {
        libc::sigaction(sig, &raw const default, std::ptr::null_mut());
        libc::raise(sig);
        *errno = saved;
    }
}

/// Writes the line once per process: file first, then stderr. A later caller
/// waits up to about 100 ms for the first to finish, then returns.
fn report(cause: &str, info: Option<&FatalInfo<'_>>) {
    if STATE
        .compare_exchange(FREE, WRITING, Ordering::Acquire, Ordering::Acquire)
        .is_err()
    {
        let pause = libc::timespec {
            tv_sec: 0,
            tv_nsec: 1_000_000,
        };
        for _ in 0..100 {
            if STATE.load(Ordering::Acquire) == DONE {
                return;
            }
            // SAFETY: a valid timespec; the remainder is not wanted.
            unsafe { libc::nanosleep(&raw const pause, std::ptr::null_mut()) };
        }
        return;
    }
    // The one buffer: the handler may run on Rust's small alternate stack.
    let mut buf = [0u8; LINE_MAX];
    let mut line = Line {
        buf: &mut buf,
        len: 0,
    };
    format_line(&mut line, cause, info, KEY.get().map(String::as_str));
    let bytes = line.buf.get(..line.len).unwrap_or(&[]);
    let fd = CRASH_FD.load(Ordering::Relaxed);
    // SAFETY: `bytes` is valid for its length; a failed write is ignored.
    unsafe {
        if fd >= 0 {
            libc::write(fd, bytes.as_ptr().cast(), bytes.len());
        }
        libc::write(libc::STDERR_FILENO, bytes.as_ptr().cast(), bytes.len());
    }
    STATE.store(DONE, Ordering::Release);
}

/// A line on the stack: nothing in the crash path may allocate.
struct Line<'a> {
    buf: &'a mut [u8; LINE_MAX],
    len: usize,
}

impl Line<'_> {
    /// Appends `bytes` whole if they fit and leave `reserve` bytes free.
    fn push(&mut self, bytes: &[u8], reserve: usize) -> bool {
        let end = self.len + bytes.len();
        match self.buf.get_mut(self.len..end) {
            Some(dst) if end + reserve <= LINE_MAX => {
                dst.copy_from_slice(bytes);
                self.len = end;
                true
            }
            _ => false,
        }
    }

    /// Appends msg text with `"`, `\`, control and invalid UTF-8 bytes
    /// escaped, leaving room for `"\n`. False once something did not fit.
    fn push_escaped(&mut self, bytes: &[u8]) -> bool {
        let hex = |b: u8| {
            let digit = |n: u8| if n < 10 { b'0' + n } else { b'a' + n - 10 };
            [b'\\', b'x', digit(b >> 4), digit(b & 0xf)]
        };
        for chunk in bytes.utf8_chunks() {
            for c in chunk.valid().chars() {
                let mut utf8 = [0u8; 4];
                let fits = match c {
                    '"' => self.push(b"\\\"", 2),
                    '\\' => self.push(b"\\\\", 2),
                    c if c.is_ascii_control() => self.push(&hex(c as u8), 2),
                    c => self.push(c.encode_utf8(&mut utf8).as_bytes(), 2),
                };
                if !fits {
                    return false;
                }
            }
            for &b in chunk.invalid() {
                if !self.push(&hex(b), 2) {
                    return false;
                }
            }
        }
        true
    }
}

impl std::fmt::Write for Line<'_> {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        if self.push(s.as_bytes(), 2) {
            Ok(())
        } else {
            Err(std::fmt::Error)
        }
    }
}

/// `[iso4-v8] fatal cause=… phase=… run=… prefix=… key="…" msg="…"\n`, absent
/// values left out; a long msg is cut so the line stays within [`LINE_MAX`].
fn format_line(line: &mut Line<'_>, cause: &str, info: Option<&FatalInfo<'_>>, key: Option<&str>) {
    let _ = write!(line, "[iso4-v8] fatal cause={cause}");
    if let Some(info) = info {
        if let Some(phase) = info.phase {
            let phase = match phase {
                FatalPhase::Run => "run",
                FatalPhase::OneOff => "oneoff",
                FatalPhase::ColdStart => "cold-start",
                FatalPhase::Idle => "idle",
                FatalPhase::Outside => "none",
            };
            let _ = write!(line, " phase={phase}");
        }
        if let Some(run) = info.run {
            let _ = write!(line, " run={run}");
        }
        if let Some(prefix) = info.prefix {
            let _ = write!(line, " prefix={prefix}");
        }
    }
    if let Some(key) = key {
        if line.push(b" key=\"", 2) {
            let _ = line.push_escaped(key.as_bytes());
            line.push(b"\"", 1);
        }
    }
    if let Some(info) = info {
        if (!info.message.is_empty() || !info.detail.is_empty()) && line.push(b" msg=\"", 2) {
            let _ = line.push_escaped(info.message)
                && (info.detail.is_empty()
                    || line.push_escaped(if info.message.is_empty() { b"(" } else { b" (" })
                        && line.push_escaped(info.detail)
                        && line.push_escaped(b")"));
            line.push(b"\"", 1);
        }
    }
    line.push(b"\n", 0);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(cause: FatalCause, phase: Option<FatalPhase>) -> FatalInfo<'static> {
        FatalInfo {
            cause,
            phase,
            run: None,
            prefix: None,
            message: b"",
            detail: b"",
        }
    }

    fn text(cause: &str, info: Option<&FatalInfo<'_>>) -> String {
        keyed_text(cause, info, None)
    }

    fn keyed_text(cause: &str, info: Option<&FatalInfo<'_>>, key: Option<&str>) -> String {
        let mut buf = [0u8; LINE_MAX];
        let mut line = Line {
            buf: &mut buf,
            len: 0,
        };
        format_line(&mut line, cause, info, key);
        String::from_utf8(line.buf[..line.len].to_vec()).unwrap()
    }

    #[test]
    fn a_full_oom_line() {
        let full = FatalInfo {
            run: Some(42),
            prefix: Some(7),
            message: b"Reached heap limit",
            detail: b"more",
            ..info(FatalCause::OomHeap, Some(FatalPhase::Run))
        };
        assert_eq!(
            text("oom-heap", Some(&full)),
            "[iso4-v8] fatal cause=oom-heap phase=run run=42 prefix=7 msg=\"Reached heap limit (more)\"\n"
        );
    }

    #[test]
    fn each_cause_and_phase() {
        assert_eq!(text("SIGSEGV", None), "[iso4-v8] fatal cause=SIGSEGV\n");
        let v8_fatal = FatalInfo {
            message: b"Check failed: x.",
            ..info(FatalCause::V8Fatal, None)
        };
        assert_eq!(
            text("v8-fatal", Some(&v8_fatal)),
            "[iso4-v8] fatal cause=v8-fatal msg=\"Check failed: x.\"\n"
        );
        for (phase, name) in [
            (FatalPhase::Run, "run"),
            (FatalPhase::OneOff, "oneoff"),
            (FatalPhase::ColdStart, "cold-start"),
            (FatalPhase::Idle, "idle"),
            (FatalPhase::Outside, "none"),
        ] {
            let i = info(FatalCause::OomOther, Some(phase));
            assert_eq!(
                text("oom-other", Some(&i)),
                format!("[iso4-v8] fatal cause=oom-other phase={name}\n")
            );
        }
    }

    #[test]
    fn missing_values_are_left_out() {
        let one_off = FatalInfo {
            run: Some(3),
            message: b"m",
            ..info(FatalCause::OomHeap, Some(FatalPhase::OneOff))
        };
        assert_eq!(
            text("oom-heap", Some(&one_off)),
            "[iso4-v8] fatal cause=oom-heap phase=oneoff run=3 msg=\"m\"\n"
        );
        let idle = FatalInfo {
            prefix: Some(u64::MAX),
            detail: b"d",
            ..info(FatalCause::OomHeap, Some(FatalPhase::Idle))
        };
        assert_eq!(
            text("oom-heap", Some(&idle)),
            "[iso4-v8] fatal cause=oom-heap phase=idle prefix=18446744073709551615 msg=\"(d)\"\n"
        );
    }

    #[test]
    fn the_key_sits_after_prefix_and_before_msg_on_every_kind_of_line() {
        let full = FatalInfo {
            run: Some(42),
            prefix: Some(7),
            message: b"Reached heap limit",
            ..info(FatalCause::OomHeap, Some(FatalPhase::Run))
        };
        assert_eq!(
            keyed_text("oom-heap", Some(&full), Some("tenant-a")),
            "[iso4-v8] fatal cause=oom-heap phase=run run=42 prefix=7 key=\"tenant-a\" msg=\"Reached heap limit\"\n"
        );
        let v8_fatal = FatalInfo {
            message: b"Check failed: x.",
            ..info(FatalCause::V8Fatal, None)
        };
        assert_eq!(
            keyed_text("v8-fatal", Some(&v8_fatal), Some("k")),
            "[iso4-v8] fatal cause=v8-fatal key=\"k\" msg=\"Check failed: x.\"\n"
        );
        assert_eq!(
            keyed_text("SIGSEGV", None, Some("k")),
            "[iso4-v8] fatal cause=SIGSEGV key=\"k\"\n"
        );
    }

    #[test]
    fn the_key_is_escaped() {
        assert_eq!(
            keyed_text("SIGABRT", None, Some("a b\"c\nd\\")),
            "[iso4-v8] fatal cause=SIGABRT key=\"a b\\\"c\\x0ad\\\\\"\n"
        );
    }

    #[test]
    fn a_long_msg_after_a_long_key_still_fits() {
        let key = "\n".repeat(200);
        let long = [b'x'; 2000];
        let i = FatalInfo {
            message: &long,
            ..info(FatalCause::OomHeap, Some(FatalPhase::Run))
        };
        let line = keyed_text("oom-heap", Some(&i), Some(&key));
        assert!(line.len() <= LINE_MAX, "{}", line.len());
        assert!(line.contains(&format!("key=\"{}\"", "\\x0a".repeat(200))));
        assert!(line.ends_with("\"\n"));
    }

    #[test]
    fn msg_is_escaped() {
        let i = FatalInfo {
            message: b"a\"b\\c\nd\x7f\xffe\xc3\xa4",
            ..info(FatalCause::V8Fatal, None)
        };
        assert_eq!(
            text("v8-fatal", Some(&i)),
            "[iso4-v8] fatal cause=v8-fatal msg=\"a\\\"b\\\\c\\x0ad\\x7f\\xffe\u{e4}\"\n"
        );
    }

    #[test]
    fn a_long_msg_is_cut_and_the_line_still_closes() {
        let long = [b'x'; 2000];
        let mut escapes = [b'"'; 2000];
        escapes[0] = b'y';
        let wide = "\u{e4}".repeat(1000);
        for message in [&long[..], &escapes[..], wide.as_bytes()] {
            let i = FatalInfo {
                message,
                detail: b"never shown",
                ..info(FatalCause::OomHeap, Some(FatalPhase::Run))
            };
            let line = text("oom-heap", Some(&i));
            assert!(
                line.len() <= LINE_MAX && line.len() >= LINE_MAX - 4,
                "{}",
                line.len()
            );
            assert!(line.ends_with("\"\n"));
            assert!(!line.contains("never shown"));
            assert_eq!(line.matches('\n').count(), 1);
        }
    }
}
