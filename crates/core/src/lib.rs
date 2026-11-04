//! iso4-core: the engine behind the `iso4-v8` binary.
//!
//! Isolates, limits, the turn loop, the warm registry, the prefix store, the
//! web globals and the value codec. It encodes no frame and owns no socket:
//! a frontend (`crates/server`) drives [`engine::Engine`] and answers through
//! [`link::HostLink`]. `publish = false`, no API stability guarantees.

pub mod blob;
pub mod container;
pub mod cpuclock;
pub mod engine;
pub mod errors;
pub mod link;
pub mod oom;
pub mod policy;
pub mod prefix;
pub mod rss;
pub mod snapshot;
#[cfg(any(test, feature = "test-support"))]
pub mod testval;
pub mod types;
pub mod url;
pub mod v8;
pub mod warm;
pub mod webcodec;
pub mod webtypes;

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
