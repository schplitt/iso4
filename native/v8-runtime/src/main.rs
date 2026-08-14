//! iso4-v8 — V8 host binary for the iso4 sandbox.
//!
//! See DESIGN.md §8 for the planned module layout and §9 for the phased
//! build plan.

use iso4_v8_runtime::{blob, session};

use std::os::unix::net::UnixListener;
use std::sync::Arc;

fn main() {
    let (socket_path, token, max_live) = parse_args();

    // Compute the V8 serialization probe (and with it this binary's write
    // format version) once, in a throwaway isolate, before any connection
    // arrives. The handshake in `session.rs` is then a byte comparison — no
    // isolate plumbing in the session layer, no per-connection cost.
    blob::probe();

    // Remove stale socket from a previous run if present.
    let _ = std::fs::remove_file(&socket_path);

    let listener = match UnixListener::bind(&socket_path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[iso4-v8] failed to bind socket at {socket_path}: {e}");
            std::process::exit(1);
        }
    };

    eprintln!("[iso4-v8] listening on {socket_path}");

    // Shared state across all connection threads: prefix snapshots, the
    // counter used to generate unique PrefixIds, and the auth token.
    let shared = Arc::new(session::SharedState::new(token, max_live));

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

fn parse_args() -> (String, String, usize) {
    let args: Vec<String> = std::env::args().collect();
    let mut socket: Option<String> = None;
    let mut token: Option<String> = None;
    let mut max_isolates: Option<usize> = None;
    let mut max_live: Option<usize> = None;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--socket" if i + 1 < args.len() => {
                socket = Some(args[i + 1].clone());
                i += 2;
            }
            "--token" if i + 1 < args.len() => {
                token = Some(args[i + 1].clone());
                i += 2;
            }
            "--max-isolates" if i + 1 < args.len() => {
                match args[i + 1].parse::<usize>() {
                    Ok(n) if n > 0 => max_isolates = Some(n),
                    _ => {
                        eprintln!(
                            "[iso4-v8] --max-isolates must be a positive integer, got {:?}",
                            args[i + 1]
                        );
                        std::process::exit(1);
                    }
                }
                i += 2;
            }
            "--max-live-isolates" if i + 1 < args.len() => {
                match args[i + 1].parse::<usize>() {
                    Ok(n) if n > 0 => max_live = Some(n),
                    _ => {
                        eprintln!(
                            "[iso4-v8] --max-live-isolates must be a positive integer, got {:?}",
                            args[i + 1]
                        );
                        std::process::exit(1);
                    }
                }
                i += 2;
            }
            arg => {
                eprintln!("[iso4-v8] unknown argument: {arg}");
                i += 1;
            }
        }
    }

    let socket = socket.unwrap_or_else(|| {
        eprintln!("[iso4-v8] --socket <path> is required");
        std::process::exit(1);
    });
    let token = token.unwrap_or_else(|| {
        eprintln!("[iso4-v8] --token <secret> is required");
        std::process::exit(1);
    });

    // The warm registry's live-isolate cap (#65): the host derives it from
    // its memory budget (`budget ÷ memoryMb`, floored at the pool size).
    // When `--max-live-isolates` is absent (old host, direct invocation)
    // fall back to the pool size — the #64 capacity model — and when that is
    // also absent, to the core count, the same default the host uses for
    // maxIsolates.
    let max_live = max_live.or(max_isolates).unwrap_or_else(|| {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
    });

    (socket, token, max_live)
}
