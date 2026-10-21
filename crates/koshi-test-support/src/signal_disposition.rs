//! The lock that tests changing the process-wide `SIGPIPE` disposition hold.

use std::sync::{Mutex, MutexGuard, PoisonError};

/// Held by each test that sets the process-wide `SIGPIPE` disposition, or
/// reads it back, including a test whose failed `exec` sets it to its default
/// and back.
static SIGPIPE_DISPOSITION_LOCK: Mutex<()> = Mutex::new(());

/// Take the lock that guards the `SIGPIPE` disposition, waiting while another
/// test holds it. A lock that a panicking test left behind is taken back. The
/// disposition is this test's own until the returned guard drops.
pub fn lock_sigpipe_disposition() -> MutexGuard<'static, ()> {
    SIGPIPE_DISPOSITION_LOCK
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}
