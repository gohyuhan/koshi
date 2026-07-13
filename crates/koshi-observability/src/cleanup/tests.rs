//! Tests for panic-safe cleanup-hook isolation and ordering in `TerminalCleanupGuard`.
//!
//! Verifies that hooks registered on the guard run in insertion order on drop, that a panic
//! triggers cleanup via the installed panic hook exactly once, and that a panicking cleanup
//! hook never aborts the process or prevents later hooks from running.

use super::*;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;

/// Returns a shared lock for serializing panic-hook tests.
///
/// Both panic-hook tests mutate the process-global panic hook. Rust runs tests in parallel by
/// default, so multiple tests setting/restoring hooks concurrently would cause one test's
/// `set_hook` call to land between another test's install and `catch_unwind`, breaking the
/// isolation. This lock ensures only one panic-hook test runs at a time.
fn panic_hook_test_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

#[test]
fn drop_runs_hooks_in_registration_order() {
    let order = Arc::new(Mutex::new(Vec::new()));
    {
        let guard = TerminalCleanupGuard::new();
        for i in 0..3 {
            let order = Arc::clone(&order);
            guard.register_cleanup(Box::new(move || order.lock().unwrap().push(i)));
        }
    } // guard drops here, running the hooks

    assert_eq!(*order.lock().unwrap(), vec![0, 1, 2]);
}

// A panic must trigger cleanup, and the subsequent drop must not run the hooks a
// second time. This test installs a process-global panic hook; it restores the
// prior hook before returning so it does not perturb other tests.
#[test]
fn panic_runs_cleanup_once_then_drop_is_noop() {
    let _serial = panic_hook_test_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let counter = Arc::new(AtomicUsize::new(0));

    // Silence the default hook so the deliberate panic below stays quiet, and
    // keep the original to restore at the end.
    let saved = panic::take_hook();
    panic::set_hook(Box::new(|_| {}));

    {
        let guard = TerminalCleanupGuard::new();
        let hook_counter = Arc::clone(&counter);
        guard.register_cleanup(Box::new(move || {
            hook_counter.fetch_add(1, Ordering::SeqCst);
        }));
        // Hold the guard for the duration: dropping it would restore the silent
        // hook and unchain the cleanup before the panic fires.
        let _panic_guard = install_panic_hook(&guard);

        let result = panic::catch_unwind(AssertUnwindSafe(|| panic!("boom")));
        assert!(result.is_err());
        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "panic hook should have run the cleanup hook"
        );
        // guard drops here: registry already drained, so nothing re-runs
    }

    assert_eq!(
        counter.load(Ordering::SeqCst),
        1,
        "drop must not re-run hooks the panic hook already ran"
    );

    panic::set_hook(saved);
}

// A hook that panics must not stop the hooks that follow it: each runs in its
// own `catch_unwind`. The deliberate panic is silenced under a no-op hook.
#[test]
fn a_panicking_hook_does_not_stop_later_hooks() {
    let _serial = panic_hook_test_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let ran = Arc::new(AtomicUsize::new(0));
    let saved = panic::take_hook();
    panic::set_hook(Box::new(|_| {}));

    {
        let guard = TerminalCleanupGuard::new();
        guard.register_cleanup(Box::new(|| panic!("first hook fails")));
        let later = Arc::clone(&ran);
        guard.register_cleanup(Box::new(move || {
            later.fetch_add(1, Ordering::SeqCst);
        }));
    } // drop runs both hooks; the first panics but is caught

    assert_eq!(
        ran.load(Ordering::SeqCst),
        1,
        "the hook after a panicking one must still run"
    );

    panic::set_hook(saved);
}

// The hardest case: a cleanup hook panics while cleanup runs *from the panic
// hook*. Run inline that aborts the process (a panic during panic handling), so
// reaching the assertions at all proves the hooks ran off the panic path. The
// hook after the panicking one must still run.
#[test]
fn a_panicking_hook_during_panic_handling_does_not_abort() {
    let _serial = panic_hook_test_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let ran = Arc::new(AtomicUsize::new(0));
    let saved = panic::take_hook();
    panic::set_hook(Box::new(|_| {}));

    {
        let guard = TerminalCleanupGuard::new();
        guard.register_cleanup(Box::new(|| panic!("cleanup hook fails")));
        let later = Arc::clone(&ran);
        guard.register_cleanup(Box::new(move || {
            later.fetch_add(1, Ordering::SeqCst);
        }));
        let _panic_guard = install_panic_hook(&guard);

        let result = panic::catch_unwind(AssertUnwindSafe(|| panic!("boom")));
        assert!(result.is_err());
    }

    assert_eq!(
        ran.load(Ordering::SeqCst),
        1,
        "a panicking hook in the panic path must not abort or skip later hooks"
    );

    panic::set_hook(saved);
}

#[test]
fn drop_with_no_registered_hooks_is_a_noop() {
    let guard = TerminalCleanupGuard::new();
    drop(guard); // must not panic on an empty registry
}

// A hook registered after a panic already drained the registry must still run
// on the guard's later normal drop: the registry is reusable, not left
// permanently drained by the earlier panic.
#[test]
fn hooks_registered_after_a_panic_drain_still_run_on_drop() {
    let _serial = panic_hook_test_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let before = Arc::new(AtomicUsize::new(0));
    let after = Arc::new(AtomicUsize::new(0));
    let saved = panic::take_hook();
    panic::set_hook(Box::new(|_| {}));

    {
        let guard = TerminalCleanupGuard::new();
        let before_counter = Arc::clone(&before);
        guard.register_cleanup(Box::new(move || {
            before_counter.fetch_add(1, Ordering::SeqCst);
        }));
        let _panic_guard = install_panic_hook(&guard);

        let result = panic::catch_unwind(AssertUnwindSafe(|| panic!("boom")));
        assert!(result.is_err());
        assert_eq!(
            before.load(Ordering::SeqCst),
            1,
            "the pre-panic hook should have run via the panic hook"
        );

        // Registry was drained by the panic; register a new hook into it.
        let after_counter = Arc::clone(&after);
        guard.register_cleanup(Box::new(move || {
            after_counter.fetch_add(1, Ordering::SeqCst);
        }));
    } // normal drop: only the post-panic hook remains registered

    assert_eq!(
        before.load(Ordering::SeqCst),
        1,
        "the pre-panic hook must not run a second time on drop"
    );
    assert_eq!(
        after.load(Ordering::SeqCst),
        1,
        "a hook registered after the panic drain must still run on drop"
    );

    panic::set_hook(saved);
}

// Dropping the `PanicHookGuard` without a panic having occurred restores the
// previously installed hook, so a later panic no longer chains into cleanup.
#[test]
fn dropping_panic_hook_guard_restores_previous_hook_so_cleanup_no_longer_chains() {
    let _serial = panic_hook_test_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let counter = Arc::new(AtomicUsize::new(0));
    let saved = panic::take_hook();
    panic::set_hook(Box::new(|_| {}));

    let guard = TerminalCleanupGuard::new();
    let hook_counter = Arc::clone(&counter);
    guard.register_cleanup(Box::new(move || {
        hook_counter.fetch_add(1, Ordering::SeqCst);
    }));

    let panic_guard = install_panic_hook(&guard);
    drop(panic_guard); // restores the silent no-op hook set above

    let result = panic::catch_unwind(AssertUnwindSafe(|| panic!("boom")));
    assert!(result.is_err());
    assert_eq!(
        counter.load(Ordering::SeqCst),
        0,
        "cleanup must not run: the panic hook was unchained before this panic fired"
    );

    panic::set_hook(saved);
    drop(guard);
}
