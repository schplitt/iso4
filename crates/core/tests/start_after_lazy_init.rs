//! A lazy V8 start already ran without the options, so `start` refuses
//! rather than skipping them (its own test binary, so its own process).

use iso4_core::engine::{Engine, EngineOptions, StartError};

#[test]
fn start_after_a_lazy_v8_start_is_refused() {
    iso4_core::v8::init_platform();
    assert_eq!(
        Engine::start(EngineOptions::default()).err(),
        Some(StartError::AlreadyStarted)
    );
}
