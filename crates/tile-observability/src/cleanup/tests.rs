use super::*;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;

// Both panic-hook tests mutate the process-global panic hook, so they must not
// run concurrently — Rust runs tests in parallel by default. Serialize them on a
// shared lock so one test's `set_hook` cannot land between another's install and
// its `catch_unwind`.
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
