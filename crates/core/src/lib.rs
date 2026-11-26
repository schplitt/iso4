//! iso4-core: the engine behind the `iso4-v8` binary.
//!
//! Isolates, limits, the turn loop, the warm registry, the prefix store, the
//! web globals and the value codec. It encodes no frame and owns no socket:
//! a frontend (`crates/server`) drives [`Engine`] and answers through
//! [`HostLink`]. `publish = false`, no API stability guarantees.

mod blob;
mod container;
mod cpuclock;
mod engine;
mod errors;
mod link;
#[doc(hidden)]
pub mod log;
mod native;
mod policy;
mod prefix;
mod rss;
mod snapshot;
#[cfg(any(test, feature = "test-support"))]
pub mod testing;
mod types;
mod url;
mod v8;
mod warm;
mod webcodec;
mod webtypes;
pub mod wire;

pub use engine::{
    AdmissionLine, Caller, Capacity, Engine, EngineOptions, PrefixInfo, Refusal, RunHandle,
    StartError,
};
pub use errors::{RunError, RuntimeErrorData};
pub use link::{
    Addr, ConsoleStream, HostLink, Inbound, LinkError, NoopObserver, Observer, Outbound,
};
pub use v8::{FatalCause, FatalHook, FatalInfo, FatalPhase, GraceReport, GraceStatus, ResetCause};
pub use warm::RegistryStats;

/// Reached by [`trace!`]; not API.
#[doc(hidden)]
pub use engine::trace_enabled;

// `testval` lives in `testing`; core's tests keep saying `crate::testval`.
#[cfg(test)]
use testing::testval;
