//! Shared environment-variable lock for tests.
//!
//! `std::env::set_var`/`remove_var` mutate process-global state, so tests
//! that touch environment variables must not run concurrently — a parallel
//! test can read a variable another test just set (or removed). Every test
//! that mutates an environment variable takes this lock for its duration.

#![cfg(test)]

use std::sync::{Mutex, MutexGuard};

static ENV_MUTEX: Mutex<()> = Mutex::new(());

/// Acquire the process-wide environment lock. Hold the guard for the whole
/// test body (and restore the previous values before dropping it).
pub fn lock() -> MutexGuard<'static, ()> {
    ENV_MUTEX
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
