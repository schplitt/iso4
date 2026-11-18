//! The pid file next to the socket, `--node-pid` and `--cores`, observed on
//! the real binary.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const TIMEOUT: Duration = Duration::from_secs(10);

/// Kills the child and removes its directory however the test ends.
struct Spawned {
    child: Child,
    dir: PathBuf,
    socket: PathBuf,
}

impl Drop for Spawned {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn fresh_dir() -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .subsec_nanos();
    let dir = std::env::temp_dir().join(format!("iso4-{}-{n}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Starts the binary with `extra` flags and waits until its socket accepts.
fn spawn(extra: &[&str]) -> (Spawned, UnixStream) {
    let dir = fresh_dir();
    let socket = dir.join("sock");
    let child = Command::new(env!("CARGO_BIN_EXE_iso4-v8"))
        .arg("--socket")
        .arg(&socket)
        .args(extra)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let spawned = Spawned { child, dir, socket };
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if let Ok(stream) = UnixStream::connect(&spawned.socket) {
            return (spawned, stream);
        }
        assert!(Instant::now() < deadline, "socket never accepted");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn run_to_exit(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_iso4-v8"))
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

/// Polls until the child exits; `None` if it is still up after `within`.
fn exit_within(child: &mut Child, within: Duration) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().unwrap() {
            return Some(status);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    None
}

fn frame(message_type: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = (payload.len() as u32 + 1).to_be_bytes().to_vec();
    out.push(message_type);
    out.extend_from_slice(payload);
    out
}

fn read_frame(stream: &mut UnixStream) -> Vec<u8> {
    let mut len = [0u8; 4];
    stream.read_exact(&mut len).unwrap();
    let mut body = vec![0u8; u32::from_be_bytes(len) as usize];
    stream.read_exact(&mut body).unwrap();
    body
}

/// The handshake from `closed_stderr.rs`: protocol 1, a format-15 null probe,
/// a 16-byte token.
fn authenticate(stream: &mut UnixStream) {
    stream.set_read_timeout(Some(TIMEOUT)).unwrap();
    let mut auth = 1u16.to_be_bytes().to_vec();
    auth.extend_from_slice(&3u32.to_be_bytes());
    auth.extend_from_slice(&[0xFF, 0x0F, 0x30]);
    auth.extend_from_slice(&16u32.to_be_bytes());
    auth.extend_from_slice(&[0xAB; 16]);
    stream.write_all(&frame(0x01, &auth)).unwrap();
    assert_eq!(read_frame(stream)[..2], [0x05, 0], "Hello, status ok");
}

/// A `Run` frame with no filename, default limits, no globals, imports or call.
fn send_run(stream: &mut UnixStream, code: &str) {
    let mut p = 1u32.to_be_bytes().to_vec();
    p.extend_from_slice(&(code.len() as u32).to_be_bytes());
    p.extend_from_slice(code.as_bytes());
    p.push(0);
    p.extend_from_slice(&[0u8; 10]);
    p.extend_from_slice(&0u32.to_be_bytes());
    p.extend_from_slice(&0u32.to_be_bytes());
    p.push(0);
    stream.write_all(&frame(0x02, &p)).unwrap();
}

#[cfg(target_os = "linux")]
fn start_value(pid: u32) -> u64 {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let rest = &stat[stat.rfind(')').unwrap() + 1..];
    rest.split_ascii_whitespace()
        .nth(19)
        .unwrap()
        .parse()
        .unwrap()
}

#[cfg(target_os = "macos")]
fn start_value(pid: u32) -> u64 {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let n = unsafe {
        libc::proc_pidinfo(
            pid as i32,
            libc::PROC_PIDTBSDINFO,
            0,
            (&raw mut info).cast(),
            size,
        )
    };
    assert_eq!(n, size);
    info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec
}

fn read_stderr(child: &mut Child) -> String {
    let mut text = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut text)
        .unwrap();
    text
}

#[test]
fn the_pid_file_names_the_child_by_the_time_the_socket_accepts() {
    let (spawned, _stream) = spawn(&[]);
    let text = std::fs::read_to_string(spawned.dir.join("pid")).unwrap();
    let line = text
        .strip_suffix('\n')
        .expect("one line ending in a newline");
    let (pid, start) = line.split_once(' ').unwrap();
    assert_eq!(pid.parse::<u32>().unwrap(), spawned.child.id());
    assert_eq!(
        start.parse::<u64>().unwrap(),
        start_value(spawned.child.id())
    );
    assert!(!spawned.dir.join("pid.tmp").exists());
}

#[test]
fn a_missing_socket_directory_fails_at_the_pid_file() {
    let dir = fresh_dir().join("missing");
    let socket = dir.join("sock");
    let out = run_to_exit(&["--socket", socket.to_str().unwrap()]);
    let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("pid file"), "{stderr}");
    assert!(
        stderr.contains(&*dir.join("pid.tmp").to_string_lossy()),
        "{stderr}"
    );
}

#[test]
fn bad_flag_values_are_fatal() {
    for (flag, value) in [("--cores", "0"), ("--cores", "x"), ("--node-pid", "x")] {
        let out = run_to_exit(&["--socket", "/unused/sock", flag, value]);
        assert_eq!(out.status.code(), Some(1), "{flag} {value}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains(&format!("{flag} must be")), "{stderr}");
    }
}

#[test]
fn cores_seeds_the_slot_allowance() {
    let (_spawned, mut stream) = spawn(&["--cores", "37"]);
    authenticate(&mut stream);
    send_run(&mut stream, "export default 1");
    let body = loop {
        let body = read_frame(&mut stream);
        if body[0] == 0x02 {
            break body;
        }
    };
    // Result: type, u32 runId, u32 slotAllowance.
    assert_eq!(u32::from_be_bytes(body[5..9].try_into().unwrap()), 37);
}

/// A throwaway process standing in for Node; killed and reaped on drop.
struct FakeNode(Child);

impl Drop for FakeNode {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn fake_node() -> FakeNode {
    FakeNode(Command::new("sleep").arg("60").spawn().unwrap())
}

#[test]
fn the_child_exits_when_node_is_a_zombie() {
    let mut node = fake_node();
    let pid = node.0.id().to_string();
    let (mut spawned, _stream) = spawn(&["--node-pid", &pid]);
    // Killed but not reaped until `node` drops: a zombie while we assert.
    node.0.kill().unwrap();
    let status = exit_within(&mut spawned.child, Duration::from_secs(3));
    assert_eq!(status.and_then(|s| s.code()), Some(0));
    assert!(read_stderr(&mut spawned.child).contains(&format!("node host pid {pid} is gone")));
}

#[test]
fn the_child_stays_up_while_node_lives() {
    let node = fake_node();
    let (mut spawned, _stream) = spawn(&["--node-pid", &node.0.id().to_string()]);
    assert_eq!(
        exit_within(&mut spawned.child, Duration::from_secs(3)),
        None
    );
}

#[test]
fn the_child_exits_with_a_run_spinning() {
    let mut node = fake_node();
    let (mut spawned, mut stream) = spawn(&["--node-pid", &node.0.id().to_string()]);
    authenticate(&mut stream);
    send_run(&mut stream, "while (true) {}");
    std::thread::sleep(Duration::from_millis(300));
    node.0.kill().unwrap();
    let status = exit_within(&mut spawned.child, Duration::from_secs(3));
    assert_eq!(status.and_then(|s| s.code()), Some(0));
}
