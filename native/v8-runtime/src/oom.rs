//! OOM-victim preference for the runtime child.
//!
//! A container OOM kill must take this process, never the Node host: the
//! child's death is failed runs plus a respawnable sandbox, the host's is
//! the whole service. A process may raise its own `oom_score_adj` without
//! privileges, so the child marks itself at startup. The capacity
//! watermarks exist to keep the container away from that line — this only
//! orders the kill if it is crossed anyway.

/// Comfortably above the host's default of 0 without claiming the scale's
/// extreme (−1000..=1000): the gap orders the kill, and an embedder running
/// other adjusted processes keeps room on either side.
pub const OOM_SCORE_ADJ: &str = "500";

/// Best-effort and Linux-only (macOS has no OOM score): a failure leaves
/// the kernel's default ordering and one stderr line, never a startup
/// failure.
pub fn prefer_this_process_as_victim() {
    #[cfg(target_os = "linux")]
    if let Err(e) = std::fs::write("/proc/self/oom_score_adj", OOM_SCORE_ADJ) {
        eprintln!("[iso4-v8] could not set oom_score_adj: {e}");
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(all(test, target_os = "linux"))]
mod tests {
    #[test]
    fn marks_this_process_as_the_preferred_victim() {
        super::prefer_this_process_as_victim();
        let score = std::fs::read_to_string("/proc/self/oom_score_adj")
            .expect("readable on any Linux with /proc");
        assert_eq!(score.trim(), super::OOM_SCORE_ADJ);
    }
}
