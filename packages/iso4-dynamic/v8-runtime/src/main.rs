//! iso4-v8 — V8 host binary for the iso4 sandbox.
//!
//! Status: scaffolding only. `main()` prints a not-implemented message and
//! exits non-zero. The planned module layout is documented in
//! `../../DESIGN.md` §8; modules will be added as their implementation
//! lands per the phased build plan in `../../DESIGN.md` §9.

mod ipc;

fn main() {
    eprintln!("iso4-v8: not yet implemented. See DESIGN.md §9 for the build plan.");
    std::process::exit(1);
}
