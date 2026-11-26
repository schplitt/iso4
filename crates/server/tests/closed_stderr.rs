//! The child keeps serving after the reader of its stderr pipe is gone.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const TIMEOUT: Duration = Duration::from_secs(10);

/// Kills the child and removes its directory however the test ends.
struct Spawned {
    child: Child,
    dir: PathBuf,
}

impl Drop for Spawned {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn frame(message_type: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = (payload.len() as u32 + 1).to_be_bytes().to_vec();
    out.push(message_type);
    out.extend_from_slice(payload);
    out
}

/// The frame's type byte and the first byte of its payload.
fn read_frame_head(stream: &mut UnixStream) -> std::io::Result<[u8; 2]> {
    let mut len = [0u8; 4];
    stream.read_exact(&mut len)?;
    let mut body = vec![0u8; u32::from_be_bytes(len) as usize];
    stream.read_exact(&mut body)?;
    Ok([body[0], body[1]])
}

#[test]
fn a_closed_stderr_pipe_does_not_end_the_session() {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .subsec_nanos();
    let dir = std::env::temp_dir().join(format!("iso4-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("sock");
    let child = Command::new(env!("CARGO_BIN_EXE_iso4-v8"))
        .arg("--socket")
        .arg(&socket)
        .args(["--id", "1"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut spawned = Spawned { child, dir };

    // The capacity line is the last one the main thread writes at startup.
    // The reader thread drops the pipe's read end once it has seen it.
    let stderr = spawned.child.stderr.take().unwrap();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let started = BufReader::new(stderr)
            .lines()
            .map_while(Result::ok)
            .any(|line| line.contains("capacity:"));
        let _ = ready_tx.send(started);
    });
    assert_eq!(
        ready_rx.recv_timeout(TIMEOUT),
        Ok(true),
        "child did not start"
    );

    let mut stream = UnixStream::connect(&socket).unwrap();
    stream.set_read_timeout(Some(TIMEOUT)).unwrap();
    // Wire constants from `src/ipc.rs`: protocol 1, role node, a format-15
    // serialized null as the probe, a 16-byte token.
    let mut auth = 1u16.to_be_bytes().to_vec();
    auth.push(0);
    auth.extend_from_slice(&3u32.to_be_bytes());
    auth.extend_from_slice(&[0xFF, 0x0F, 0x30]);
    auth.extend_from_slice(&16u32.to_be_bytes());
    auth.extend_from_slice(&[0xAB; 16]);
    stream.write_all(&frame(0x01, &auth)).unwrap();
    assert_eq!(
        read_frame_head(&mut stream).ok(),
        Some([0x05, 0]),
        "Hello, status ok"
    );

    // The session logs the handshake to the dead pipe before it reads this.
    stream.write_all(&frame(0x08, &[])).unwrap();
    assert_eq!(
        read_frame_head(&mut stream).ok().map(|h| h[0]),
        Some(0x06),
        "StatsResult"
    );
}
