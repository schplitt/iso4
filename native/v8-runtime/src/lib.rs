//! iso4-v8 library target.
//!
//! The product is the `iso4-v8` binary (`src/main.rs`); this lib target
//! exists so `benches/` (criterion, `cargo bench`) can reach the value codec
//! and the V8 conversion helpers. It is `publish = false` and has no public
//! API stability guarantees — nothing outside this repository links it.

pub mod blob;
pub mod ipc;
pub mod oom;
pub mod policy;
pub mod rss;
pub mod session;
#[cfg(test)]
pub mod testval;
pub mod url;
pub mod v8;
pub mod warm;
pub mod webcodec;
pub mod webtypes;
pub mod wire;
