//! iso4-v8 library target.
//!
//! The product is the `iso4-v8` binary (`src/main.rs`); this lib target
//! exists so `benches/` (criterion, `cargo bench`) can reach the value codec
//! and the V8 conversion helpers. It is `publish = false` and has no public
//! API stability guarantees — nothing outside this repository links it.

pub mod blob;
pub mod container;
pub mod cpuclock;
pub mod errors;
pub mod ipc;
pub mod oom;
pub mod policy;
pub mod rss;
pub mod session;
pub mod snapshot;
#[cfg(test)]
pub mod testval;
pub mod types;
pub mod url;
pub mod v8;
pub mod warm;
pub mod webcodec;
pub mod webtypes;
pub mod wire;

/// The startup snapshot `build.rs` creates (see `snapshot.rs`).
pub static SNAPSHOT_BLOB: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/snapshot.bin"));

extern "C" {
    fn iso4_new_frozen_clock_platform(thread_pool_size: std::ffi::c_int) -> *mut ::v8::Platform;
}

/// The default platform with V8's wall clock routed to the entered isolate's
/// frozen clock (`src/clock_platform.cc`). `build.rs` defines its own stock
/// version of this function.
pub(crate) fn new_platform() -> ::v8::UniqueRef<::v8::Platform> {
    // SAFETY: a fresh heap-allocated platform the caller now owns.
    unsafe { ::v8::UniquePtr::from_raw(iso4_new_frozen_clock_platform(0)) }.unwrap()
}
