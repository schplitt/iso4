//! iso4-v8 — V8 host binary for the iso4 sandbox.
//!
//! See DESIGN.md §8 for the planned module layout and §9 for the phased
//! build plan.

mod crash;
mod ipc;
mod oom;
mod process;
mod session;
mod wire;

use iso4_core::{blob, container, engine, policy, rss, v8 as sandbox, warm};
use iso4_types as types;

use std::os::unix::net::UnixListener;
use std::sync::Arc;

fn main() {
    // If the container ever OOMs, the kernel must take this process, not
    // the Node host.
    oom::prefer_this_process_as_victim();

    let (socket_path, warm_budget_bytes, host_reserve_bytes, idle_settle_secs, cores, node_pid) =
        parse_args();

    // Before V8 starts (`blob::probe` below), so a death in its start is recorded.
    crash::install(&socket_path);

    // Before any line is drawn: the admission line is derived from it.
    container::set_host_reserve_bytes(host_reserve_bytes);
    sandbox::set_idle_settle_secs(idle_settle_secs);
    // Before the V8 platform starts (`blob::probe` below): it sizes its pool from this.
    if let Some(n) = cores {
        warm::set_cores(n);
    }
    if let Some(pid) = node_pid {
        process::watch_node(pid);
    }

    // A budget with no readable meter would silently never be enforced —
    // fail at startup instead (rationale: DESIGN.md §13.2.1).
    if warm_budget_bytes > 0
        && container::usage_bytes().is_none()
        && rss::process_rss_bytes().is_none()
    {
        iso4_core::log!(
            "[iso4-v8] cannot read container memory or this process's \
             resident memory, so the {warm_budget_bytes}-byte memory budget \
             could never be enforced. This usually means neither the cgroup \
             filesystem nor /proc is readable in this environment. Mount \
             one, or pass memoryBudgetMb: 0 to run without a budget."
        );
        std::process::exit(1);
    }

    // 0 when no container limit is readable (line disabled).
    let hard_line_bytes = container::admission_line_bytes();

    // Compute the V8 serialization probe (and with it this binary's write
    // format version) once, in a throwaway isolate, before any connection
    // arrives. The handshake in `session.rs` is then a byte comparison — no
    // isolate plumbing in the session layer, no per-connection cost.
    blob::probe();

    if let Err(e) = process::write_pid_file(&socket_path) {
        iso4_core::log!("[iso4-v8] {e}");
        std::process::exit(1);
    }

    // No pre-bind cleanup: the host hands this process a fresh path inside a
    // per-sandbox private directory, so a file already at it means two
    // children were given the same path. Deleting it would hide that bug;
    // failing the bind surfaces it.
    let listener = match UnixListener::bind(&socket_path) {
        Ok(l) => l,
        Err(e) => {
            iso4_core::log!("[iso4-v8] failed to bind socket at {socket_path}: {e}");
            std::process::exit(1);
        }
    };

    iso4_core::log!("[iso4-v8] listening on {socket_path}");

    // One line answering what a capacity refusal makes an operator ask:
    // where the lines sit and what derived them.
    const MB: u64 = 1024 * 1024;
    let budget = if warm_budget_bytes == 0 {
        "disabled".to_string()
    } else {
        format!("{} MB", warm_budget_bytes / MB)
    };
    let default_heap_mb = types::DEFAULT_MEMORY_MB;
    let warm_ceiling_mb = default_heap_mb + policy::heap_band_mb(default_heap_mb);
    match container::limit_info() {
        Some((limit, source)) => iso4_core::log!(
            "[iso4-v8] capacity: container limit {} MB ({source}), host reserve {} MB, \
             admission line {} MB, warm budget {budget}, default run heap \
             {default_heap_mb} MB (warm-run ceiling {warm_ceiling_mb} MB)",
            limit / MB,
            container::host_reserve_bytes() / MB,
            hard_line_bytes / MB,
        ),
        None => iso4_core::log!(
            "[iso4-v8] capacity: container limit unreadable — admission line disabled; \
             warm budget {budget}, default run heap {default_heap_mb} MB \
             (warm-run ceiling {warm_ceiling_mb} MB)"
        ),
    }

    // One engine for all connections. Access control is the owner-only
    // directory the host created the socket path in.
    let engine = Arc::new(engine::Engine::new(
        warm_budget_bytes,
        hard_line_bytes,
    ));

    // Cold start: one run per core until a completed run reports its shape.
    warm::seed_slot_allowance(warm::cores());

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let engine = Arc::clone(&engine);
                std::thread::spawn(move || session::handle_client(stream, engine));
            }
            Err(e) => {
                iso4_core::log!("[iso4-v8] accept error: {e}");
                break;
            }
        }
    }
}

fn parse_args() -> (String, u64, u64, u64, Option<usize>, Option<i32>) {
    let args: Vec<String> = std::env::args().collect();
    let mut socket: Option<String> = None;
    let mut warm_budget_bytes: u64 = 0;
    let mut host_reserve_bytes: u64 = container::DEFAULT_HOST_RESERVE_BYTES;
    let mut idle_settle_secs: u64 = 0;
    let mut cores: Option<usize> = None;
    let mut node_pid: Option<i32> = None;

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
                        iso4_core::log!(
                            "[iso4-v8] --warm-budget-bytes must be a non-negative integer, got {:?}",
                            args[i + 1]
                        );
                        std::process::exit(1);
                    }
                }
                i += 2;
            }
            "--host-reserve-bytes" if i + 1 < args.len() => {
                // What the container limit owes the Node host before any
                // line is drawn. 0 is valid (no reserve); absent keeps the
                // default the host mirrors.
                match args[i + 1].parse::<u64>() {
                    Ok(n) => host_reserve_bytes = n,
                    Err(_) => {
                        iso4_core::log!(
                            "[iso4-v8] --host-reserve-bytes must be a non-negative integer, got {:?}",
                            args[i + 1]
                        );
                        std::process::exit(1);
                    }
                }
                i += 2;
            }
            "--idle-settle-secs" if i + 1 < args.len() => {
                // Opt-in: an instance idle this long collects once, then
                // parks. 0 or absent leaves it off.
                match args[i + 1].parse::<u64>() {
                    Ok(n) => idle_settle_secs = n,
                    Err(_) => {
                        iso4_core::log!(
                            "[iso4-v8] --idle-settle-secs must be a non-negative integer, got {:?}",
                            args[i + 1]
                        );
                        std::process::exit(1);
                    }
                }
                i += 2;
            }
            "--cores" if i + 1 < args.len() => {
                // Replaces the machine's core count; absent keeps it.
                match args[i + 1].parse::<usize>() {
                    Ok(n) if n >= 1 => cores = Some(n),
                    _ => {
                        iso4_core::log!(
                            "[iso4-v8] --cores must be an integer of at least 1, got {:?}",
                            args[i + 1]
                        );
                        std::process::exit(1);
                    }
                }
                i += 2;
            }
            "--node-pid" if i + 1 < args.len() => {
                // Exit once this pid is gone.
                match args[i + 1].parse::<i32>() {
                    Ok(n) if n >= 1 => node_pid = Some(n),
                    _ => {
                        iso4_core::log!(
                            "[iso4-v8] --node-pid must be a positive integer, got {:?}",
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
                iso4_core::log!("[iso4-v8] unknown argument: {arg}");
                std::process::exit(1);
            }
        }
    }

    let socket = socket.unwrap_or_else(|| {
        iso4_core::log!("[iso4-v8] --socket <path> is required");
        std::process::exit(1);
    });

    (
        socket,
        warm_budget_bytes,
        host_reserve_bytes,
        idle_settle_secs,
        cores,
        node_pid,
    )
}
