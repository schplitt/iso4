//! iso4-v8 — V8 host binary for the iso4 sandbox.
//!
//! See DESIGN.md §8 for the planned module layout and §9 for the phased
//! build plan.

mod ipc;
mod session;
mod v8;
mod wire;

use std::os::unix::net::UnixListener;
use std::sync::Arc;

fn main() {
    let socket_path = "/tmp/iso4-dynamic-v8.sock";

    // Remove stale socket from a previous run if present.
    let _ = std::fs::remove_file(socket_path);

    let listener = match UnixListener::bind(socket_path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[iso4-v8] failed to bind socket at {socket_path}: {e}");
            std::process::exit(1);
        }
    };

    eprintln!("[iso4-v8] listening on {socket_path}");

    // Shared state across all connection threads: prefix snapshots and the
    // counter used to generate unique PrefixIds.
    let shared = Arc::new(session::SharedState::new());

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let shared = Arc::clone(&shared);
                std::thread::spawn(move || session::handle_client(stream, shared));
            }
            Err(e) => {
                eprintln!("[iso4-v8] accept error: {e}");
                break;
            }
        }
    }
}
