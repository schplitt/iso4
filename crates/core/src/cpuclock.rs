//! Per-thread CPU clocks — what a thread actually burned on a core, not how
//! long it was alive.
//!
//! A [`ThreadClock`] is captured on the thread it measures and readable from
//! any other thread, because the CPU guard's watchdog polls a budget it does
//! not own. Both platforms' readings are the same counter the target thread
//! sees for itself; measured to be within 7 µs on Linux and 3 µs on macOS.
//!
//! Cost per read, measured (release, arm64, uncontended): 131 ns on Linux,
//! 350 ns on macOS, against ~23 ns for `Instant::now()`. Two reads bracket
//! each turn, so the bracket uses [`self_ns`] — the same counter by the
//! cheaper route when the reader IS the thread (186 ns on macOS; on Linux it
//! is the identical call). Only the watchdog needs a [`ThreadClock`].

/// A handle to one thread's CPU clock, readable from any thread.
///
/// Linux: the thread's POSIX CPU clock id. macOS: the thread's mach port,
/// which `pthread_mach_thread_np` returns without taking a reference — so
/// there is nothing to release.
#[derive(Clone, Copy, Debug)]
pub struct ThreadClock(imp::Handle);

impl ThreadClock {
    /// Capture the calling thread's clock.
    pub fn current() -> Self {
        Self(imp::current())
    }

    /// Nanoseconds of user + system CPU this thread has burned, or 0 if the
    /// thread is gone. Callers subtract with `saturating_sub`, so a failed
    /// read contributes no time rather than a wrapped one.
    pub fn read_ns(self) -> u64 {
        imp::read_ns(self.0)
    }
}

/// The calling thread's own CPU nanoseconds — the same counter a
/// [`ThreadClock`] on this thread reports, by the cheaper same-thread route.
pub fn self_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) } != 0 {
        return 0;
    }
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

#[cfg(target_os = "linux")]
mod imp {
    pub type Handle = libc::clockid_t;

    pub fn current() -> Handle {
        let mut id: libc::clockid_t = 0;
        // Only fails for a thread that has already exited.
        if unsafe { libc::pthread_getcpuclockid(libc::pthread_self(), &mut id) } != 0 {
            return libc::CLOCK_THREAD_CPUTIME_ID;
        }
        id
    }

    pub fn read_ns(h: Handle) -> u64 {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        if unsafe { libc::clock_gettime(h, &mut ts) } != 0 {
            return 0;
        }
        ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
    }
}

#[cfg(target_os = "macos")]
mod imp {
    pub type Handle = libc::mach_port_t;

    pub fn current() -> Handle {
        unsafe { libc::pthread_mach_thread_np(libc::pthread_self()) }
    }

    pub fn read_ns(h: Handle) -> u64 {
        let mut info = std::mem::MaybeUninit::<libc::thread_basic_info>::uninit();
        let mut count = libc::THREAD_BASIC_INFO_COUNT;
        let kr = unsafe {
            libc::thread_info(
                h as libc::thread_inspect_t,
                libc::THREAD_BASIC_INFO as libc::thread_flavor_t,
                info.as_mut_ptr().cast(),
                &mut count,
            )
        };
        if kr != 0 {
            return 0;
        }
        // `time_value_t` is whole seconds plus microseconds, for user and
        // system separately.
        let i = unsafe { info.assume_init() };
        (i.user_time.seconds as u64 + i.system_time.seconds as u64) * 1_000_000_000
            + (i.user_time.microseconds as u64 + i.system_time.microseconds as u64) * 1_000
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!(
    "no per-thread CPU clock for this target; cpuTimeMs would silently \
     report wall time (iso4 ships linux-gnu and macOS only)"
);

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    #[test]
    fn a_burning_thread_accrues_roughly_wall_time() {
        let clock = ThreadClock::current();
        let before = clock.read_ns();
        let wall = Instant::now();
        let mut x = 0u64;
        while wall.elapsed() < Duration::from_millis(50) {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
        }
        std::hint::black_box(x);
        let burned = clock.read_ns() - before;
        // Uncontended this is ~50 ms; a loaded CI box only lowers it.
        assert!(
            (1_000_000..=60_000_000).contains(&burned),
            "implausible CPU for a 50 ms spin: {burned} ns"
        );
    }

    #[test]
    fn a_sleeping_thread_accrues_almost_nothing() {
        let clock = ThreadClock::current();
        let before = clock.read_ns();
        std::thread::sleep(Duration::from_millis(200));
        let burned = clock.read_ns() - before;
        assert!(burned < 20_000_000, "sleep charged {burned} ns of CPU");
    }

    #[test]
    fn another_thread_can_read_the_clock_of_a_busy_thread() {
        let (tx, rx) = std::sync::mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let s = Arc::clone(&stop);
        let worker = std::thread::spawn(move || {
            tx.send(ThreadClock::current()).unwrap();
            let mut x = 0u64;
            while !s.load(Ordering::Relaxed) {
                x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
            }
            x
        });
        let clock = rx.recv().unwrap();

        let before = clock.read_ns();
        std::thread::sleep(Duration::from_millis(100));
        let burned = clock.read_ns() - before;
        stop.store(true, Ordering::Relaxed);
        worker.join().unwrap();
        assert!(
            burned > 10_000_000,
            "remote read saw only {burned} ns while the target spun 100 ms"
        );
    }
}
