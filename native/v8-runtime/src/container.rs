//! Global container memory — the meter and the line every capacity mark
//! compares against.
//!
//! The watermarks guard the CONTAINER, not this process: the OOM killer
//! charges the whole cgroup (Node host included), so metering the child's
//! own RSS would leave the real ceiling unwatched (AGENTS.md "Memory
//! watermarks measure GLOBAL container memory"). Usage is the cgroup's
//! working set — `memory.current − inactive_file` — because reclaimable
//! page cache never causes an OOM kill and must not cause evictions or
//! refusals either (celld meters the same way). Where no cgroup exists
//! (macOS, bare metal), callers fall back to the child's own RSS via
//! `rss.rs`: a weaker meter, but the only one available there.
//!
//! Reads follow the container-runtime convention (celld, Docker, K8s): the
//! container's cgroup is mounted at `/sys/fs/cgroup` inside its namespace,
//! so the files are read at the mount root — no `/proc/self/cgroup` walk.

use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(target_os = "linux")]
use std::sync::atomic::AtomicI32;
use std::sync::OnceLock;
use std::time::Instant;

/// Reserved for the Node host + this runtime's own overhead when deriving
/// the admission line from the container limit. Mirrored by the host's
/// budget derivation (`index.ts` `defaultMemoryBudgetMb`) — change both
/// together. A measured reserve replaces this constant later (#165).
pub const NODE_RESERVE_BYTES: u64 = 256 * 1024 * 1024;

/// The hard admission line: 90% of (container limit − Node reserve). An
/// isolate is only ever created while measured usage + the run's own heap
/// cap stays at or below it, so the newest admission can never be what
/// tips the container over (#77 ruling). `0` = no limit readable, line
/// disabled.
pub fn admission_line_bytes() -> u64 {
    match limit_bytes() {
        Some(limit) => limit.saturating_sub(NODE_RESERVE_BYTES) / 10 * 9,
        None => 0,
    }
}

/// The container's memory limit, read once: cgroup v2 → cgroup v1 → host
/// total. `None` only when even the host total is unreadable.
pub fn limit_bytes() -> Option<u64> {
    static LIMIT: OnceLock<Option<u64>> = OnceLock::new();
    *LIMIT.get_or_init(read_limit)
}

/// Measured global usage (the cgroup working set), or `None` when no
/// cgroup exists — the caller falls back to child RSS. Cached for
/// [`USAGE_TTL_MS`]: sampling happens per registry event (potentially every
/// ~50 µs warm call), and the two-file cgroup read with the `memory.stat`
/// scan is too expensive at that cadence. The staleness window is covered
/// by the admission line's built-in headroom (one worst-case isolate).
pub fn usage_bytes() -> Option<u64> {
    const USAGE_TTL_MS: u64 = 100;
    // u64::MAX = "no cached value"; the value cell holds usage+1 so a real
    // reading of 0 is distinguishable from the sentinel.
    static CACHED_AT_MS: AtomicU64 = AtomicU64::new(u64::MAX);
    static CACHED: AtomicU64 = AtomicU64::new(0);
    static ANCHOR: OnceLock<Instant> = OnceLock::new();

    let anchor = *ANCHOR.get_or_init(Instant::now);
    let now_ms = anchor.elapsed().as_millis() as u64;
    let at = CACHED_AT_MS.load(Ordering::Acquire);
    if at != u64::MAX && now_ms.saturating_sub(at) < USAGE_TTL_MS {
        let cached = CACHED.load(Ordering::Acquire);
        return (cached != 0).then(|| cached - 1);
    }
    let fresh = read_usage();
    CACHED.store(fresh.map_or(0, |v| v + 1), Ordering::Release);
    CACHED_AT_MS.store(now_ms, Ordering::Release);
    fresh
}

#[cfg(target_os = "linux")]
fn read_limit() -> Option<u64> {
    // cgroup v2: "max" or bytes. v1: a near-2^63 sentinel means unlimited
    // (treat anything ≥ 2^50 as no limit, like Node's constrainedMemory).
    if let Ok(s) = std::fs::read_to_string("/sys/fs/cgroup/memory.max") {
        match s.trim().parse::<u64>() {
            Ok(n) if n < 1 << 50 => return Some(n),
            _ => {} // "max" or sentinel: fall through to the host total
        }
    }
    if let Ok(s) = std::fs::read_to_string("/sys/fs/cgroup/memory/memory.limit_in_bytes") {
        if let Ok(n) = s.trim().parse::<u64>() {
            if n < 1 << 50 {
                return Some(n);
            }
        }
    }
    // No container limit: the machine is the container.
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    let kb: u64 = meminfo
        .lines()
        .find(|l| l.starts_with("MemTotal:"))?
        .split_ascii_whitespace()
        .nth(1)?
        .parse()
        .ok()?;
    Some(kb * 1024)
}

#[cfg(target_os = "macos")]
fn read_limit() -> Option<u64> {
    let mut size: u64 = 0;
    let mut len = std::mem::size_of::<u64>();
    let name = c"hw.memsize";
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            (&mut size as *mut u64).cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    (rc == 0 && size > 0).then_some(size)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn read_limit() -> Option<u64> {
    None
}

/// Fd cache for the per-sample reads, same retry discipline as
/// `rss::STATM_FD`: −1 means "not opened yet or last attempt failed", so a
/// transient open failure never blinds the meter permanently.
#[cfg(target_os = "linux")]
fn cached_fd(cell: &AtomicI32, path: &std::ffi::CStr) -> Option<i32> {
    let cached = cell.load(Ordering::Acquire);
    if cached >= 0 {
        return Some(cached);
    }
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
    if fd < 0 {
        return None;
    }
    match cell.compare_exchange(-1, fd, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => Some(fd),
        Err(winner) => {
            unsafe { libc::close(fd) };
            Some(winner)
        }
    }
}

#[cfg(target_os = "linux")]
fn pread_string(fd: i32, buf: &mut [u8]) -> Option<usize> {
    let n = unsafe { libc::pread(fd, buf.as_mut_ptr().cast(), buf.len() - 1, 0) };
    (n > 0).then_some(n as usize)
}

#[cfg(target_os = "linux")]
fn read_usage() -> Option<u64> {
    static CURRENT_FD: AtomicI32 = AtomicI32::new(-1);
    static STAT_FD: AtomicI32 = AtomicI32::new(-1);
    static V1: AtomicI32 = AtomicI32::new(-1); // v1 fallback fds, lazily
    static V1_STAT: AtomicI32 = AtomicI32::new(-1);

    let (current, inactive) = if let Some(fd) = cached_fd(&CURRENT_FD, c"/sys/fs/cgroup/memory.current")
    {
        let stat = cached_fd(&STAT_FD, c"/sys/fs/cgroup/memory.stat");
        (read_number(fd)?, stat.and_then(|s| read_stat_field(s, "inactive_file ")))
    } else if let Some(fd) = cached_fd(&V1, c"/sys/fs/cgroup/memory/memory.usage_in_bytes") {
        let stat = cached_fd(&V1_STAT, c"/sys/fs/cgroup/memory/memory.stat");
        (read_number(fd)?, stat.and_then(|s| read_stat_field(s, "total_inactive_file ")))
    } else {
        return None;
    };
    // Working set: page cache the kernel can reclaim never OOMs the
    // container, so it must not evict warmth or refuse admissions.
    Some(current.saturating_sub(inactive.unwrap_or(0)))
}

#[cfg(target_os = "linux")]
fn read_number(fd: i32) -> Option<u64> {
    let mut buf = [0u8; 32];
    let n = pread_string(fd, &mut buf)?;
    std::str::from_utf8(&buf[..n]).ok()?.trim().parse().ok()
}

#[cfg(target_os = "linux")]
fn read_stat_field(fd: i32, key: &str) -> Option<u64> {
    let mut buf = [0u8; 4096];
    let n = pread_string(fd, &mut buf)?;
    let text = std::str::from_utf8(&buf[..n]).ok()?;
    text.lines()
        .find(|l| l.starts_with(key))?
        .split_ascii_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

#[cfg(not(target_os = "linux"))]
fn read_usage() -> Option<u64> {
    None
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limit_is_plausible_on_supported_platforms() {
        let limit = limit_bytes().expect("Linux and macOS always resolve a total");
        assert!(limit > 256 * 1024 * 1024, "implausibly small: {limit}");
        assert!(limit < 1 << 50, "unlimited sentinel leaked through: {limit}");
    }

    #[test]
    fn admission_line_sits_below_the_limit() {
        let line = admission_line_bytes();
        let limit = limit_bytes().unwrap();
        assert!(line > 0);
        assert!(line < limit, "line {line} must leave headroom below {limit}");
        assert_eq!(line, limit.saturating_sub(NODE_RESERVE_BYTES) / 10 * 9);
    }

    #[test]
    fn usage_is_none_or_plausible() {
        // macOS/bare metal: None (callers fall back to child RSS). In a
        // container: a real working-set number below the limit sentinel.
        if let Some(usage) = usage_bytes() {
            assert!(usage > 0);
            assert!(usage < 1 << 50);
        }
    }
}
