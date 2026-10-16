//! Builds the V8 startup snapshot (`src/snapshot.rs`) into `OUT_DIR`, where
//! `src/lib.rs` embeds it. The runtime modules are compiled into this script
//! so the snapshot's native callbacks are the binary's own.
#![allow(warnings)]

#[path = "src/blob.rs"]
mod blob;
#[path = "src/container.rs"]
mod container;
#[path = "src/cpuclock.rs"]
mod cpuclock;
#[path = "src/ipc.rs"]
mod ipc;
#[path = "src/oom.rs"]
mod oom;
#[path = "src/policy.rs"]
mod policy;
#[path = "src/rss.rs"]
mod rss;
#[path = "src/session.rs"]
mod session;
#[path = "src/snapshot.rs"]
mod snapshot;
#[path = "src/url.rs"]
mod url;
#[path = "src/v8.rs"]
mod v8;
#[path = "src/warm.rs"]
mod warm;
#[path = "src/webcodec.rs"]
mod webcodec;
#[path = "src/webtypes.rs"]
mod webtypes;
#[path = "src/wire.rs"]
mod wire;

// Read only when an isolate boots, which never happens in this script.
static SNAPSHOT_BLOB: &[u8] = &[];

fn main() {
    println!("cargo:rerun-if-changed=src");
    let out = std::path::Path::new(&std::env::var_os("OUT_DIR").unwrap()).join("snapshot.bin");
    std::fs::write(out, snapshot::create()).unwrap();
}
