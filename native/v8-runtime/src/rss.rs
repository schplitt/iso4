//! Process RSS (resident set size) — the memory-pressure signal for #66.
//!
//! Read in THIS process: the watermarks compare the runtime's own footprint
//! against the warm budget (Node's `process.memoryUsage().rss` would measure
//! the wrong process). RSS rather than summed heap numbers because heap
//! undercounts what the OS actually charges — external ArrayBuffers, V8
//! overhead, allocator fragmentation, later SQLite — and the container OOM
//! killer acts on the OS's number, not ours.
//!
//! Cost, measured 2026-08-14 (release, idle M-series): ~0.4 µs per macOS
//! `task_info` read — ~0.8 ‰ of a warm call per sampled event, which is why
//! sampling happens per registry event and iso4 needs no polling timer
//! (celld samples on a 1 s ticker instead; same numbers, different clock).

/// This process's resident set size in bytes, or `None` where no reader
/// exists (unsupported OS, or `/proc` reads failing) — callers treat `None`
/// as "watermarks unavailable", never as zero.
pub fn process_rss_bytes() -> Option<u64> {
    imp::read()
}

#[cfg(target_os = "macos")]
mod imp {
    /// `mach_task_basic_info` (flavor `MACH_TASK_BASIC_INFO`): the kernel's
    /// own accounting for this task; `resident_size` is in bytes already.
    #[repr(C)]
    #[derive(Default)]
    struct MachTaskBasicInfo {
        virtual_size: u64,
        resident_size: u64,
        resident_size_max: u64,
        user_time_s: i32,
        user_time_us: i32,
        system_time_s: i32,
        system_time_us: i32,
        policy: i32,
        suspend_count: i32,
    }

    const MACH_TASK_BASIC_INFO: u32 = 20;
    const MACH_TASK_BASIC_INFO_COUNT: u32 =
        (std::mem::size_of::<MachTaskBasicInfo>() / std::mem::size_of::<u32>()) as u32;

    extern "C" {
        static mach_task_self_: u32;
        fn task_info(task: u32, flavor: u32, info: *mut i32, count: *mut u32) -> i32;
    }

    pub fn read() -> Option<u64> {
        let mut info = MachTaskBasicInfo::default();
        let mut count = MACH_TASK_BASIC_INFO_COUNT;
        let kr = unsafe {
            task_info(
                mach_task_self_,
                MACH_TASK_BASIC_INFO,
                (&mut info as *mut MachTaskBasicInfo).cast::<i32>(),
                &mut count,
            )
        };
        (kr == 0).then_some(info.resident_size)
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::sync::OnceLock;

    /// `/proc/self/statm`, held open for the process's life and `pread` at
    /// offset 0 per sample — procfs regenerates the content per read, and
    /// skipping open/close keeps the cost near the macOS number. `pread` is
    /// position-less per call, so concurrent samplers need no lock.
    static STATM_FD: OnceLock<Option<i32>> = OnceLock::new();
    static PAGE_SIZE: OnceLock<u64> = OnceLock::new();

    pub fn read() -> Option<u64> {
        let fd = (*STATM_FD.get_or_init(|| {
            let path = c"/proc/self/statm";
            let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
            (fd >= 0).then_some(fd)
        }))?;
        let page_size = *PAGE_SIZE
            .get_or_init(|| unsafe { libc::sysconf(libc::_SC_PAGESIZE).max(0) as u64 });

        // statm: "size resident shared text lib data dt" in pages; the
        // whole line fits well under 128 bytes.
        let mut buf = [0u8; 128];
        let n = unsafe { libc::pread(fd, buf.as_mut_ptr().cast(), buf.len() - 1, 0) };
        if n <= 0 {
            return None;
        }
        let text = std::str::from_utf8(&buf[..n as usize]).ok()?;
        let resident_pages: u64 = text.split_ascii_whitespace().nth(1)?.parse().ok()?;
        Some(resident_pages * page_size)
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod imp {
    pub fn read() -> Option<u64> {
        None
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_a_plausible_resident_size() {
        let rss = process_rss_bytes().expect("supported OS must report RSS");
        // A running Rust test binary is at least a couple of MB resident
        // and far below a petabyte; catches unit mixups (pages vs bytes).
        assert!(rss > 1024 * 1024, "implausibly small: {rss}");
        assert!(rss < 1 << 50, "implausibly large: {rss}");
    }

    #[test]
    fn repeated_reads_agree_roughly() {
        // Two immediate samples of an idle process differ by well under 2× —
        // guards against returning uninitialized or shifted fields.
        let a = process_rss_bytes().unwrap();
        let b = process_rss_bytes().unwrap();
        assert!(a.abs_diff(b) < a / 2, "unstable samples: {a} vs {b}");
    }
}
