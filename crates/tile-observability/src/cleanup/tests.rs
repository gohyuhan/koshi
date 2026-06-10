use super::*;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicUsize, Ordering};

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
        install_panic_hook(&guard);

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
