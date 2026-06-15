//! Shared test doubles for `aura-telemetry` unit tests. Compiled only
//! under `cfg(test)`.

use std::collections::HashMap;

use crate::disable::EnvProvider;

/// In-memory [`EnvProvider`] so tests don't read the process environment
/// (which leaks across parallel `cargo test` threads). Build one with
/// `MockEnv::new()` / `MockEnv::default()` and chain `.set(..)`.
#[derive(Default)]
pub(crate) struct MockEnv(HashMap<String, String>);

impl MockEnv {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn set(mut self, key: &str, value: &str) -> Self {
        self.0.insert(key.to_string(), value.to_string());
        self
    }
}

impl EnvProvider for MockEnv {
    fn var(&self, key: &str) -> Option<String> {
        self.0.get(key).cloned()
    }
}
