//! iso4-v8 — V8 host binary for the iso4 sandbox.
//!
//! See DESIGN.md §8 for the planned module layout and §9 for the phased
//! build plan.

use iso4_v8_runtime::{blob, rss, session};

use std::os::unix::net::UnixListener;
use std::sync::Arc;

fn main() {
    let (socket_path, warm_budget_bytes) = parse_args();

    // The warm budget is enforced by comparing this process's own resident
    // memory against it, so a budget with no way to read that memory is not a
    // budget at all: nothing would ever be evicted, warm instances would
    // accumulate for as long as the process lived, and `stats()` would report
    // a healthy idle runtime throughout. Refusing to start says so at the
    // first call instead — the host reports this exit immediately, and the
    // line below is on its stderr. A budget of 0 means watermarks are off by
    // request, so nothing is read and nothing is checked.
    if warm_budget_bytes > 0 && rss::process_rss_bytes().is_none() {
        eprintln!(
            "[iso4-v8] cannot read this process's resident memory, so the \
             {warm_budget_bytes}-byte memory budget could never be enforced. \
             This usually means /proc is not mounted or is not readable in \
             this environment. Mount it, or pass memoryBudgetMb: 0 to run \
             without a budget."
        );
        std::process::exit(1);
    }

    // Compute the V8 serialization probe (and with it this binary's write
    // format version) once, in a throwaway isolate, before any connection
    // arrives. The handshake in `session.rs` is then a byte comparison — no
    // isolate plumbing in the session layer, no per-connection cost.
    blob::probe();

    // No pre-bind cleanup: the host hands this process a fresh path inside a
    // per-sandbox private directory, so a file already at it means two
    // children were given the same path. Deleting it would hide that bug;
    // failing the bind surfaces it.
    let listener = match UnixListener::bind(&socket_path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[iso4-v8] failed to bind socket at {socket_path}: {e}");
            std::process::exit(1);
        }
    };

    eprintln!("[iso4-v8] listening on {socket_path}");

    // Shared state across all connection threads: prefix snapshots and the
    // counter used to generate unique PrefixIds. Access control is the
    // owner-only directory the host created the socket path in — the kernel
    // checks it on every connect, so no application-level secret is needed.
    let shared = Arc::new(session::SharedState::new(warm_budget_bytes));

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

fn parse_args() -> (String, u64) {
    let args: Vec<String> = std::env::args().collect();
    let mut socket: Option<String> = None;
    let mut warm_budget_bytes: u64 = 0;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--socket" if i + 1 < args.len() => {
                socket = Some(args[i + 1].clone());
                i += 2;
            }
            "--warm-budget-bytes" if i + 1 < args.len() => {
                // The one RSS mark the registry sheds against. 0 is
                // valid and means watermarks off; absent means the same.
                // Concurrency needs no flag — the host pool bounds it, and
                // there is no instance-count cap (celld's stance).
                match args[i + 1].parse::<u64>() {
                    Ok(n) => warm_budget_bytes = n,
                    Err(_) => {
                        eprintln!(
                            "[iso4-v8] --warm-budget-bytes must be a non-negative integer, got {:?}",
                            args[i + 1]
                        );
                        std::process::exit(1);
                    }
                }
                i += 2;
            }
            arg => {
                // Fatal, like every other bad input here. Continuing would
                // leave a mistyped `--warm-budget-bytes` at its initial 0,
                // which means watermarks off — the memory ceiling silently
                // absent, with one stderr line to say so.
                eprintln!("[iso4-v8] unknown argument: {arg}");
                std::process::exit(1);
            }
        }
    }

    let socket = socket.unwrap_or_else(|| {
        eprintln!("[iso4-v8] --socket <path> is required");
        std::process::exit(1);
    });

    (socket, warm_budget_bytes)
}
