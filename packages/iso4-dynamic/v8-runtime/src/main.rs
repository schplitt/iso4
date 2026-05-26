//! iso4-v8 — V8 host binary for the iso4 sandbox.
//!
//! See DESIGN.md §8 for the planned module layout and §9 for the phased
//! build plan.

mod ipc;
mod session;
mod v8;
mod wire;

use std::os::unix::net::UnixListener;

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

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                std::thread::spawn(|| session::handle_client(stream));
            }
            Err(e) => {
                eprintln!("[iso4-v8] accept error: {e}");
                break;
            }
        }
    }
}
