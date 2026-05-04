//! Process-wide lock for tests that read or mutate environment variables.
//!
//! Cargo runs all `#[test]` functions in a crate as parallel threads inside a
//! single binary, but `std::env::set_var` / `remove_var` mutate global process
//! state. Tests that touch the env therefore race with each other. Acquire
//! this lock at the top of any such test to serialize them.

use std::sync::{Mutex, MutexGuard};

static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Acquire the process-wide env-mutation lock for this test binary.
///
/// Recovers from poisoning so a panicking test does not cascade into every
/// later test in the binary failing to acquire the lock.
pub(crate) fn lock() -> MutexGuard<'static, ()> {
    ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner())
}
