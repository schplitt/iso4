//! `Engine::start` runs once per process (its own test binary, so its own process).

use iso4_core::engine::{Engine, EngineOptions, StartError};

#[test]
fn a_second_start_is_refused() {
    assert!(Engine::start(EngineOptions::default()).is_ok());
    assert_eq!(
        Engine::start(EngineOptions::default()).err(),
        Some(StartError::AlreadyStarted)
    );
}
