//! This process's `pid` file and the watch on the Node host's pid.

use std::path::Path;
use std::time::Duration;

/// One read of a pid's process record. The start value is opaque: it is only
/// compared for equality on this machine.
enum Record {
    Start(u64),
    Zombie,
    Unreadable,
}

/// Writes `<pid> <start>\n` to `pid` next to the socket, via `pid.tmp`.
pub fn write_pid_file(socket_path: &str) -> Result<(), String> {
    let dir = Path::new(socket_path).parent().unwrap_or(Path::new(""));
    let path = dir.join("pid");
    let tmp = dir.join("pid.tmp");
    let pid = std::process::id();
    let Record::Start(start) = imp::record(pid as i32) else {
        return Err(format!(
            "cannot read this process's start time for the pid file at {}",
            path.display()
        ));
    };
    std::fs::write(&tmp, format!("{pid} {start}\n"))
        .map_err(|e| format!("failed to write the pid file at {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path)
        .map_err(|e| format!("failed to rename the pid file to {}: {e}", path.display()))
}

/// Exits this process once `pid` is gone, checked once a second.
pub fn watch_node(pid: i32) {
    let start = match imp::record(pid) {
        Record::Start(s) => Some(s),
        Record::Zombie | Record::Unreadable => None,
    };
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(1));
        if node_is_gone(pid, start) {
            iso4_core::log!("[iso4-v8] node host pid {pid} is gone, exiting");
            std::process::exit(0);
        }
    });
}

/// Dead, zombie or reused. A record that cannot be read counts as alive.
fn node_is_gone(pid: i32, start: Option<u64>) -> bool {
    // SAFETY: signal 0 sends nothing; it only checks that the pid exists.
    if unsafe { libc::kill(pid, 0) } != 0 {
        return std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
    }
    match imp::record(pid) {
        Record::Zombie => true,
        Record::Start(now) => start.is_some_and(|s| s != now),
        Record::Unreadable => false,
    }
}

/// `/proc/<pid>/stat` split after the last `)` (comm may hold spaces and
/// `)`): the state (field 3) and starttime (field 22).
#[cfg(any(target_os = "linux", test))]
fn parse_stat(line: &str) -> Option<(char, u64)> {
    let (_, rest) = line.rsplit_once(')')?;
    let mut fields = rest.split_ascii_whitespace();
    let state = fields.next()?.chars().next()?;
    let start = fields.nth(18)?.parse().ok()?;
    Some((state, start))
}

#[cfg(target_os = "linux")]
mod imp {
    use super::Record;

    pub fn record(pid: i32) -> Record {
        // Bytes, not a String: comm may end in a cut UTF-8 character.
        let Ok(bytes) = std::fs::read(format!("/proc/{pid}/stat")) else {
            return Record::Unreadable;
        };
        match super::parse_stat(&String::from_utf8_lossy(&bytes)) {
            Some(('Z' | 'X', _)) => Record::Zombie,
            Some((_, start)) => Record::Start(start),
            None => Record::Unreadable,
        }
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use super::Record;

    pub fn record(pid: i32) -> Record {
        // SAFETY: proc_bsdinfo is plain integers and byte arrays; all zeros is valid.
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        // SAFETY: `info` is a writable buffer of exactly `size` bytes.
        let n = unsafe {
            libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size)
        };
        if n == size {
            return Record::Start(info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec);
        }
        // A pid that `kill` still reaches but has no BSD info is a zombie.
        match std::io::Error::last_os_error().raw_os_error() {
            Some(libc::ESRCH) => Record::Zombie,
            _ => Record::Unreadable,
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod imp {
    use super::Record;

    pub fn record(_pid: i32) -> Record {
        Record::Unreadable
    }
}

#[cfg(test)]
mod tests {
    use super::parse_stat;

    fn line(comm: &str, state: char) -> String {
        // Fields 4..=21 are filler; field 22 (starttime) is 4242.
        let filler = (4..=21)
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join(" ");
        format!("123 ({comm}) {state} {filler} 4242 999 1000\n")
    }

    #[test]
    fn reads_state_and_start_time() {
        assert_eq!(parse_stat(&line("node", 'S')), Some(('S', 4242)));
    }

    #[test]
    fn a_paren_and_space_in_comm_do_not_shift_the_fields() {
        assert_eq!(parse_stat(&line("a) b", 'R')), Some(('R', 4242)));
    }

    #[test]
    fn reads_a_zombie_state() {
        assert_eq!(parse_stat(&line("node", 'Z')), Some(('Z', 4242)));
    }

    #[test]
    fn a_comm_cut_inside_a_utf8_character_still_parses() {
        let mut bytes = b"123 (".to_vec();
        bytes.extend_from_slice(&[0xc3, 0xa4].repeat(7));
        bytes.push(0xc3);
        let tail = line("x", 'Z');
        bytes.extend_from_slice(&tail.as_bytes()[tail.find(')').unwrap()..]);
        let text = String::from_utf8_lossy(&bytes);
        assert_eq!(parse_stat(&text), Some(('Z', 4242)));
    }

    #[test]
    fn a_truncated_line_is_none() {
        let full = line("node", 'S');
        let cut = &full[..full.find(" 4242").unwrap()];
        assert_eq!(parse_stat(cut), None);
        assert_eq!(parse_stat("123 (node"), None);
    }
}
