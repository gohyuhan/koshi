//! Tests for `TerminalCleanupGuard` and the panic hook it arms: hooks run in
//! registration order on drop, a panic runs them exactly once through the
//! installed hook, and a panicking hook neither aborts the process nor stops
//! the hooks after it.
//!
//! Then the crash report: what the file is named and what it holds, that the
//! cleanup hooks run before it is written, and that every way the write can
//! fail leaves no file and still restores the terminal.

use super::*;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;

/// Returns a shared lock that serializes the panic-hook tests.
///
/// Every test that installs a panic hook mutates the process-global hook slot.
/// Rust runs tests in parallel, so a second test's `set_hook` can land between
/// the first test's install and its `catch_unwind`. This lock keeps one such
/// test running at a time.
fn get_panic_hook_test_lock() -> &'static Mutex<()> {
    static PANIC_HOOK_TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    PANIC_HOOK_TEST_LOCK.get_or_init(|| Mutex::new(()))
}

#[test]
fn drop_runs_hooks_in_registration_order() {
    let cleanup_order = Arc::new(Mutex::new(Vec::new()));
    {
        let cleanup_guard = TerminalCleanupGuard::new();
        for hook_index in 0..3 {
            let cleanup_order = Arc::clone(&cleanup_order);
            cleanup_guard.register_cleanup(Box::new(move || {
                cleanup_order.lock().unwrap().push(hook_index)
            }));
        }
    } // cleanup_guard drops here, running the hooks

    assert_eq!(*cleanup_order.lock().unwrap(), vec![0, 1, 2]);
}

// A panic must trigger cleanup, and the subsequent drop must not run the hooks a
// second time. This test installs a process-global panic hook; it restores the
// prior hook before returning so it does not perturb other tests.
#[test]
fn panic_runs_cleanup_once_then_drop_is_noop() {
    let _panic_hook_test_guard = get_panic_hook_test_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let cleanup_count = Arc::new(AtomicUsize::new(0));

    // Silence the default hook so the deliberate panic below stays quiet, and
    // keep the original to restore at the end.
    let previous_panic_hook = panic::take_hook();
    panic::set_hook(Box::new(|_| {}));

    {
        let cleanup_guard = TerminalCleanupGuard::new();
        let cleanup_count_for_hook = Arc::clone(&cleanup_count);
        cleanup_guard.register_cleanup(Box::new(move || {
            cleanup_count_for_hook.fetch_add(1, Ordering::SeqCst);
        }));
        // Hold the guard for the duration: dropping it would restore the silent
        // hook and unchain the cleanup before the panic fires.
        let _panic_hook_guard = install_panic_hook(&cleanup_guard, None);

        let panic_result = panic::catch_unwind(AssertUnwindSafe(|| panic!("boom")));
        assert_eq!(
            panic_result.unwrap_err().downcast_ref::<&str>(),
            Some(&"boom")
        );
        assert_eq!(
            cleanup_count.load(Ordering::SeqCst),
            1,
            "panic hook should have run the cleanup hook"
        );
        // cleanup_guard drops here: registry already drained, so nothing re-runs
    }

    assert_eq!(
        cleanup_count.load(Ordering::SeqCst),
        1,
        "drop must not re-run hooks the panic hook already ran"
    );

    panic::set_hook(previous_panic_hook);
}

// A hook that panics must not stop the hooks that follow it: each runs in its
// own `catch_unwind`. The deliberate panic is silenced under a no-op hook.
#[test]
fn a_panicking_hook_does_not_stop_later_hooks() {
    let _panic_hook_test_guard = get_panic_hook_test_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let completed_hook_count = Arc::new(AtomicUsize::new(0));
    let previous_panic_hook = panic::take_hook();
    panic::set_hook(Box::new(|_| {}));

    {
        let cleanup_guard = TerminalCleanupGuard::new();
        cleanup_guard.register_cleanup(Box::new(|| panic!("first hook fails")));
        let following_hook_count = Arc::clone(&completed_hook_count);
        cleanup_guard.register_cleanup(Box::new(move || {
            following_hook_count.fetch_add(1, Ordering::SeqCst);
        }));
    } // drop runs both hooks; the first panics but is caught

    assert_eq!(
        completed_hook_count.load(Ordering::SeqCst),
        1,
        "the hook after a panicking one must still run"
    );

    panic::set_hook(previous_panic_hook);
}

// The hardest case: a cleanup hook panics while cleanup runs *from the panic
// hook*. The hooks run on a fresh thread, so reaching the assertions at all
// proves the hooks ran off the panic path. The hook after the panicking one
// must still run.
#[test]
fn a_panicking_hook_during_panic_handling_does_not_abort() {
    let _panic_hook_test_guard = get_panic_hook_test_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let completed_hook_count = Arc::new(AtomicUsize::new(0));
    let previous_panic_hook = panic::take_hook();
    panic::set_hook(Box::new(|_| {}));

    {
        let cleanup_guard = TerminalCleanupGuard::new();
        cleanup_guard.register_cleanup(Box::new(|| panic!("cleanup hook fails")));
        let following_hook_count = Arc::clone(&completed_hook_count);
        cleanup_guard.register_cleanup(Box::new(move || {
            following_hook_count.fetch_add(1, Ordering::SeqCst);
        }));
        let _panic_hook_guard = install_panic_hook(&cleanup_guard, None);

        let panic_result = panic::catch_unwind(AssertUnwindSafe(|| panic!("boom")));
        assert_eq!(
            panic_result.unwrap_err().downcast_ref::<&str>(),
            Some(&"boom")
        );
    }

    assert_eq!(
        completed_hook_count.load(Ordering::SeqCst),
        1,
        "a panicking hook in the panic path must not abort or skip following hooks"
    );

    panic::set_hook(previous_panic_hook);
}

#[test]
fn drop_with_no_registered_hooks_is_a_noop() {
    let cleanup_guard = TerminalCleanupGuard::new();
    drop(cleanup_guard); // must not panic on an empty registry
}

#[test]
fn a_default_guard_runs_its_hooks_on_drop() {
    let cleanup_count = Arc::new(AtomicUsize::new(0));
    {
        let cleanup_guard = TerminalCleanupGuard::default();
        let cleanup_count_for_hook = Arc::clone(&cleanup_count);
        cleanup_guard.register_cleanup(Box::new(move || {
            cleanup_count_for_hook.fetch_add(1, Ordering::SeqCst);
        }));
    }

    assert_eq!(cleanup_count.load(Ordering::SeqCst), 1);
}

// A thread that dies while it holds the registry poisons that lock. The
// terminal must still be restored: registering and draining both recover the
// poisoned lock instead of panicking.
#[test]
fn cleanup_still_runs_after_a_thread_died_holding_the_registry() {
    let _panic_hook_test_guard = get_panic_hook_test_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let before_cleanup_count = Arc::new(AtomicUsize::new(0));
    let after_cleanup_count = Arc::new(AtomicUsize::new(0));

    let cleanup_guard = TerminalCleanupGuard::new();
    let before_cleanup_count_for_hook = Arc::clone(&before_cleanup_count);
    cleanup_guard.register_cleanup(Box::new(move || {
        before_cleanup_count_for_hook.fetch_add(1, Ordering::SeqCst);
    }));

    // Silence the default hook so the deliberate panic below stays quiet.
    let previous_panic_hook = panic::take_hook();
    panic::set_hook(Box::new(|_| {}));
    let cleanup_hook_registry = Arc::clone(&cleanup_guard.cleanup_hooks);
    let registry_thread_join_result = std::thread::spawn(move || {
        let _registry_guard = cleanup_hook_registry
            .lock()
            .expect("the registry is not poisoned yet");
        panic!("the thread holding the registry died");
    });
    let panic_payload = registry_thread_join_result
        .join()
        .expect_err("the spawned thread must have died");
    assert_eq!(
        panic_payload.downcast_ref::<&str>(),
        Some(&"the thread holding the registry died")
    );
    panic::set_hook(previous_panic_hook);

    // Registering into the poisoned registry still works.
    let after_cleanup_count_for_hook = Arc::clone(&after_cleanup_count);
    cleanup_guard.register_cleanup(Box::new(move || {
        after_cleanup_count_for_hook.fetch_add(1, Ordering::SeqCst);
    }));

    drop(cleanup_guard);

    assert_eq!(
        before_cleanup_count.load(Ordering::SeqCst),
        1,
        "a hook registered before the poisoning must still run"
    );
    assert_eq!(
        after_cleanup_count.load(Ordering::SeqCst),
        1,
        "a hook registered after the poisoning must still run"
    );
}

// A hook registered after a panic already drained the registry must still run
// on the guard's subsequent normal drop: the registry is reusable, not left
// permanently drained by the earlier panic.
#[test]
fn hooks_registered_after_a_panic_drain_still_run_on_drop() {
    let _panic_hook_test_guard = get_panic_hook_test_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let before_cleanup_count = Arc::new(AtomicUsize::new(0));
    let after_cleanup_count = Arc::new(AtomicUsize::new(0));
    let previous_panic_hook = panic::take_hook();
    panic::set_hook(Box::new(|_| {}));

    {
        let cleanup_guard = TerminalCleanupGuard::new();
        let before_cleanup_count_for_hook = Arc::clone(&before_cleanup_count);
        cleanup_guard.register_cleanup(Box::new(move || {
            before_cleanup_count_for_hook.fetch_add(1, Ordering::SeqCst);
        }));
        let _panic_hook_guard = install_panic_hook(&cleanup_guard, None);

        let panic_result = panic::catch_unwind(AssertUnwindSafe(|| panic!("boom")));
        assert_eq!(
            panic_result.unwrap_err().downcast_ref::<&str>(),
            Some(&"boom")
        );
        assert_eq!(
            before_cleanup_count.load(Ordering::SeqCst),
            1,
            "the pre-panic hook should have run via the panic hook"
        );

        // Registry was drained by the panic; register a new hook into it.
        let after_cleanup_count_for_hook = Arc::clone(&after_cleanup_count);
        cleanup_guard.register_cleanup(Box::new(move || {
            after_cleanup_count_for_hook.fetch_add(1, Ordering::SeqCst);
        }));
    } // normal drop: only the post-panic hook remains registered

    assert_eq!(
        before_cleanup_count.load(Ordering::SeqCst),
        1,
        "the pre-panic hook must not run a second time on drop"
    );
    assert_eq!(
        after_cleanup_count.load(Ordering::SeqCst),
        1,
        "a hook registered after the panic drain must still run on drop"
    );

    panic::set_hook(previous_panic_hook);
}

// Dropping the `PanicHookGuard` without a panic having occurred restores the
// previously installed hook, so a subsequent panic no longer chains into cleanup.
#[test]
fn dropping_panic_hook_guard_restores_previous_hook_so_cleanup_no_longer_chains() {
    let _panic_hook_test_guard = get_panic_hook_test_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let cleanup_count = Arc::new(AtomicUsize::new(0));
    let previous_panic_hook = panic::take_hook();
    panic::set_hook(Box::new(|_| {}));

    let cleanup_guard = TerminalCleanupGuard::new();
    let cleanup_count_for_hook = Arc::clone(&cleanup_count);
    cleanup_guard.register_cleanup(Box::new(move || {
        cleanup_count_for_hook.fetch_add(1, Ordering::SeqCst);
    }));

    let panic_hook_guard = install_panic_hook(&cleanup_guard, None);
    drop(panic_hook_guard); // restores the silent no-op hook set above

    let panic_result = panic::catch_unwind(AssertUnwindSafe(|| panic!("boom")));
    assert_eq!(
        panic_result.unwrap_err().downcast_ref::<&str>(),
        Some(&"boom")
    );
    assert_eq!(
        cleanup_count.load(Ordering::SeqCst),
        0,
        "cleanup must not run: the panic hook was unchained before this panic fired"
    );

    panic::set_hook(previous_panic_hook);
    drop(cleanup_guard);
}

// Two guards installed one inside the other. Dropping the inner one puts the
// outer chained hook back; dropping the outer one puts the original hook back.
#[test]
fn nested_panic_hook_guards_dropped_last_in_first_out_restore_each_previous_hook() {
    let _panic_hook_test_guard = get_panic_hook_test_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let original_hook_call_count = Arc::new(AtomicUsize::new(0));
    let outer_cleanups = Arc::new(AtomicUsize::new(0));
    let inner_cleanups = Arc::new(AtomicUsize::new(0));
    let previous_panic_hook = panic::take_hook();
    let original_hook_call_count_for_hook = Arc::clone(&original_hook_call_count);
    panic::set_hook(Box::new(move |_| {
        original_hook_call_count_for_hook.fetch_add(1, Ordering::SeqCst);
    }));

    let outer_cleanup_guard = TerminalCleanupGuard::new();
    let inner_cleanup_guard = TerminalCleanupGuard::new();
    let outer_cleanup_count = Arc::clone(&outer_cleanups);
    outer_cleanup_guard.register_cleanup(Box::new(move || {
        outer_cleanup_count.fetch_add(1, Ordering::SeqCst);
    }));
    let inner_cleanup_count = Arc::clone(&inner_cleanups);
    inner_cleanup_guard.register_cleanup(Box::new(move || {
        inner_cleanup_count.fetch_add(1, Ordering::SeqCst);
    }));
    let outer_panic_hook_guard = install_panic_hook(&outer_cleanup_guard, None);
    let inner_panic_hook_guard = install_panic_hook(&inner_cleanup_guard, None);

    drop(inner_panic_hook_guard);
    let panic_result = panic::catch_unwind(AssertUnwindSafe(|| panic!("boom")));
    assert_eq!(
        panic_result.unwrap_err().downcast_ref::<&str>(),
        Some(&"boom")
    );
    assert_eq!(
        inner_cleanups.load(Ordering::SeqCst),
        0,
        "inner is unchained"
    );
    assert_eq!(
        outer_cleanups.load(Ordering::SeqCst),
        1,
        "outer is still chained"
    );
    assert_eq!(original_hook_call_count.load(Ordering::SeqCst), 1);

    // A fresh outer hook shows whether outer is still chained after its guard drops.
    let outer_cleanup_count = Arc::clone(&outer_cleanups);
    outer_cleanup_guard.register_cleanup(Box::new(move || {
        outer_cleanup_count.fetch_add(1, Ordering::SeqCst);
    }));
    drop(outer_panic_hook_guard);
    let panic_result = panic::catch_unwind(AssertUnwindSafe(|| panic!("again")));
    assert_eq!(
        panic_result.unwrap_err().downcast_ref::<&str>(),
        Some(&"again")
    );
    assert_eq!(
        outer_cleanups.load(Ordering::SeqCst),
        1,
        "outer is unchained"
    );
    assert_eq!(
        original_hook_call_count.load(Ordering::SeqCst),
        2,
        "the original hook is back in place"
    );

    panic::set_hook(previous_panic_hook);
    drop(inner_cleanup_guard);
    drop(outer_cleanup_guard);
}

// The previously installed hook runs after the cleanup hooks: when it fires,
// the cleanup hook has already counted.
#[test]
fn the_previous_panic_hook_runs_after_the_cleanup_hooks() {
    let _panic_hook_test_guard = get_panic_hook_test_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let cleanup_count = Arc::new(AtomicUsize::new(0));
    let observed_count_by_previous_hook = Arc::new(Mutex::new(None));
    let previous_panic_hook = panic::take_hook();
    let watched_cleanup_count = Arc::clone(&cleanup_count);
    let observed_count = Arc::clone(&observed_count_by_previous_hook);
    panic::set_hook(Box::new(move |_| {
        *observed_count.lock().unwrap() = Some(watched_cleanup_count.load(Ordering::SeqCst));
    }));

    {
        let cleanup_guard = TerminalCleanupGuard::new();
        let cleanup_count_for_hook = Arc::clone(&cleanup_count);
        cleanup_guard.register_cleanup(Box::new(move || {
            cleanup_count_for_hook.fetch_add(1, Ordering::SeqCst);
        }));
        let _panic_hook_guard = install_panic_hook(&cleanup_guard, None);
        let panic_result = panic::catch_unwind(AssertUnwindSafe(|| panic!("boom")));
        assert_eq!(
            panic_result.unwrap_err().downcast_ref::<&str>(),
            Some(&"boom")
        );
    }

    // Read the value out and put the original hook back before asserting: the
    // installed hook takes this same lock, so a failing assertion that still
    // held it would block instead of reporting.
    let observed_cleanup_count = *observed_count_by_previous_hook.lock().unwrap();
    panic::set_hook(previous_panic_hook);

    assert_eq!(
        observed_cleanup_count,
        Some(1),
        "the previous hook ran once, after the cleanup hook"
    );
}

// A `PanicHookGuard` dropped while its thread is unwinding restores nothing:
// the chained hook stays installed, and the next panic still drains the
// registry.
#[test]
fn a_panic_hook_guard_dropped_while_unwinding_leaves_the_chained_hook_installed() {
    let _panic_hook_test_guard = get_panic_hook_test_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let cleanup_count = Arc::new(AtomicUsize::new(0));
    let previous_panic_hook = panic::take_hook();
    panic::set_hook(Box::new(|_| {}));

    let cleanup_guard = TerminalCleanupGuard::new();
    let panic_hook_guard = install_panic_hook(&cleanup_guard, None);
    let panic_result = panic::catch_unwind(AssertUnwindSafe(|| {
        let _dropped_while_unwinding = panic_hook_guard;
        panic!("boom")
    }));
    assert_eq!(
        panic_result.unwrap_err().downcast_ref::<&str>(),
        Some(&"boom")
    );

    let cleanup_count_for_hook = Arc::clone(&cleanup_count);
    cleanup_guard.register_cleanup(Box::new(move || {
        cleanup_count_for_hook.fetch_add(1, Ordering::SeqCst);
    }));
    let panic_result = panic::catch_unwind(AssertUnwindSafe(|| panic!("again")));
    assert_eq!(
        panic_result.unwrap_err().downcast_ref::<&str>(),
        Some(&"again")
    );
    assert_eq!(
        cleanup_count.load(Ordering::SeqCst),
        1,
        "the chained hook still drains the registry"
    );

    panic::set_hook(previous_panic_hook);
    drop(cleanup_guard);
}

// A guard dropped while its thread unwinds still runs its hooks, and runs them
// on a fresh thread rather than on the unwinding one.
#[test]
fn a_guard_dropped_while_unwinding_runs_its_hooks_on_another_thread() {
    let _panic_hook_test_guard = get_panic_hook_test_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let cleanup_thread_id = Arc::new(Mutex::new(None));
    let previous_panic_hook = panic::take_hook();
    panic::set_hook(Box::new(|_| {}));

    let cleanup_thread_id_for_hook = Arc::clone(&cleanup_thread_id);
    let panic_result = panic::catch_unwind(AssertUnwindSafe(move || {
        let cleanup_guard = TerminalCleanupGuard::new();
        cleanup_guard.register_cleanup(Box::new(move || {
            *cleanup_thread_id_for_hook.lock().unwrap() = Some(std::thread::current().id());
        }));
        panic!("boom")
    }));
    assert_eq!(
        panic_result.unwrap_err().downcast_ref::<&str>(),
        Some(&"boom")
    );

    panic::set_hook(previous_panic_hook);

    let cleanup_thread_id = cleanup_thread_id
        .lock()
        .unwrap()
        .expect("the hook ran while the thread unwound");
    assert_ne!(
        cleanup_thread_id,
        std::thread::current().id(),
        "the hook runs off the unwinding thread"
    );
}

// A hook that registers another hook while it runs does not block on the
// registry. The new hook is not part of the drain that is running; the next
// drain runs it.
#[test]
fn a_hook_registered_by_a_running_hook_runs_on_the_next_drain() {
    let _panic_hook_test_guard = get_panic_hook_test_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let cleanup_count = Arc::new(AtomicUsize::new(0));
    let previous_panic_hook = panic::take_hook();
    panic::set_hook(Box::new(|_| {}));

    {
        let cleanup_guard = TerminalCleanupGuard::new();
        let cleanup_hook_registry = Arc::clone(&cleanup_guard.cleanup_hooks);
        let cleanup_count_for_hook = Arc::clone(&cleanup_count);
        cleanup_guard.register_cleanup(Box::new(move || {
            cleanup_hook_registry
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(Box::new(move || {
                    cleanup_count_for_hook.fetch_add(1, Ordering::SeqCst);
                }));
        }));
        let _panic_hook_guard = install_panic_hook(&cleanup_guard, None);

        let panic_result = panic::catch_unwind(AssertUnwindSafe(|| panic!("boom")));
        assert_eq!(
            panic_result.unwrap_err().downcast_ref::<&str>(),
            Some(&"boom")
        );
        assert_eq!(
            cleanup_count.load(Ordering::SeqCst),
            0,
            "the hook registered during the drain is not part of it"
        );
    } // drop drains again: the hook the first hook registered runs now

    assert_eq!(cleanup_count.load(Ordering::SeqCst), 1);

    panic::set_hook(previous_panic_hook);
}

// --- The crash report ---

/// A fresh directory for one crash-report test, removed if a previous run
/// left it behind. The tag keeps parallel tests from sharing a directory.
fn build_crash_directory_path(crash_directory_tag: &str) -> PathBuf {
    let crash_directory_path = std::env::temp_dir().join(format!(
        "koshi-crash-{}-{crash_directory_tag}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&crash_directory_path);
    crash_directory_path
}

/// A report with every field fixed, so its file name and text are known.
fn build_fixed_crash_report() -> CrashReport {
    CrashReport {
        timestamp: 1_700_000_000,
        message: "boom".to_string(),
        location: "src/main.rs:10:5".to_string(),
        backtrace: "frame one\nframe two".to_string(),
    }
}

/// The text of the one crash report under `crash_directory_path`. Panics when the directory
/// holds anything other than exactly one file.
fn read_single_crash_report(crash_directory_path: &Path) -> String {
    let mut crash_file_paths: Vec<PathBuf> = std::fs::read_dir(crash_directory_path)
        .expect("the crash directory exists")
        .map(|directory_entry| {
            directory_entry
                .expect("the directory entry is readable")
                .path()
        })
        .collect();
    crash_file_paths.sort();
    assert_eq!(
        crash_file_paths.len(),
        1,
        "expected one crash report, found {crash_file_paths:?}"
    );
    std::fs::read_to_string(&crash_file_paths[0]).expect("the crash report is readable")
}

#[test]
fn a_crash_report_writes_a_file_named_by_its_timestamp_holding_every_fact() {
    let crash_directory_path = build_crash_directory_path("every-fact");

    build_fixed_crash_report().write_crash_report(&crash_directory_path);

    let report_text = std::fs::read_to_string(crash_directory_path.join("crash-1700000000.txt"))
        .expect("the report is written under its timestamp");
    assert_eq!(
        report_text,
        format!(
            "version: {}\nplatform: {} {}\ntimestamp: 1700000000\nmessage: boom\nlocation: src/main.rs:10:5\nbacktrace:\nframe one\nframe two\n",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    );

    let _ = std::fs::remove_dir_all(&crash_directory_path);
}

#[test]
fn a_second_report_with_the_same_timestamp_replaces_the_first() {
    let crash_directory_path = build_crash_directory_path("same-second");
    let mut second_crash_report = build_fixed_crash_report();
    second_crash_report.message = "second report".to_string();

    build_fixed_crash_report().write_crash_report(&crash_directory_path);
    second_crash_report.write_crash_report(&crash_directory_path);

    assert_eq!(
        read_single_crash_report(&crash_directory_path),
        format!(
            "version: {}\nplatform: {} {}\ntimestamp: 1700000000\nmessage: second report\nlocation: src/main.rs:10:5\nbacktrace:\nframe one\nframe two\n",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    );

    let _ = std::fs::remove_dir_all(&crash_directory_path);
}

#[test]
fn a_crash_report_creates_the_directories_leading_to_its_file() {
    let crash_directory_path = build_crash_directory_path("missing-parents")
        .join("nested")
        .join("deeper");

    build_fixed_crash_report().write_crash_report(&crash_directory_path);

    assert!(
        crash_directory_path.join("crash-1700000000.txt").is_file(),
        "the report is written under the directories it created"
    );

    let _ = std::fs::remove_dir_all(build_crash_directory_path("missing-parents"));
}

#[test]
fn a_crash_report_whose_directory_cannot_be_created_writes_nothing() {
    // The crash directory's path is already a file, so creating it fails.
    let base_directory_path = build_crash_directory_path("dir-is-a-file");
    std::fs::create_dir_all(&base_directory_path).expect("create the base directory");
    let blocked_path = base_directory_path.join("not-a-directory");
    std::fs::write(&blocked_path, b"i am a file").expect("write the blocking file");

    build_fixed_crash_report().write_crash_report(&blocked_path);

    assert_eq!(
        std::fs::read_to_string(&blocked_path).expect("the blocking file is readable"),
        "i am a file",
        "the blocking file is left untouched"
    );
    assert_eq!(
        std::fs::read_dir(&base_directory_path)
            .expect("the base directory is readable")
            .count(),
        1,
        "nothing else was written"
    );

    let _ = std::fs::remove_dir_all(&base_directory_path);
}

#[test]
fn a_crash_report_whose_file_path_is_a_directory_writes_nothing() {
    // The directory is writable, but the report's own file name is taken by
    // a directory, so the write itself fails.
    let crash_directory_path = build_crash_directory_path("file-is-a-directory");
    let taken_path = crash_directory_path.join("crash-1700000000.txt");
    std::fs::create_dir_all(&taken_path).expect("create the blocking directory");

    build_fixed_crash_report().write_crash_report(&crash_directory_path);

    assert!(
        taken_path.is_dir(),
        "the blocking directory is left in place"
    );
    assert_eq!(
        std::fs::read_dir(&taken_path)
            .expect("the blocking directory is readable")
            .count(),
        0,
        "nothing was written into it"
    );

    let _ = std::fs::remove_dir_all(&crash_directory_path);
}

// Only Unix can make a directory read-only through `std`. On Windows a failing
// write is covered by
// `a_crash_report_whose_file_path_is_a_directory_writes_nothing`.
#[cfg(unix)]
#[test]
fn a_read_only_crash_directory_this_user_owns_is_made_private_and_takes_the_report() {
    use std::os::unix::fs::PermissionsExt;

    let crash_directory_path = build_crash_directory_path("read-only");
    std::fs::create_dir_all(&crash_directory_path).expect("create the crash directory");
    std::fs::set_permissions(
        &crash_directory_path,
        std::fs::Permissions::from_mode(0o500),
    )
    .expect("make the directory read-only");

    build_fixed_crash_report().write_crash_report(&crash_directory_path);

    assert_eq!(
        std::fs::metadata(&crash_directory_path)
            .expect("the crash directory exists")
            .permissions()
            .mode()
            & 0o777,
        0o700,
        "the directory this user owns is set to owner-only"
    );
    assert_eq!(
        read_single_crash_report(&crash_directory_path),
        build_fixed_crash_report().render_crash_report_text()
    );

    let _ = std::fs::remove_dir_all(&crash_directory_path);
}

#[cfg(unix)]
#[test]
fn a_crash_directory_this_user_cannot_make_private_takes_no_report() {
    // The path names a file, so no directory can be created there.
    let crash_directory_path = build_crash_directory_path("not-a-directory");
    std::fs::create_dir_all(crash_directory_path.parent().expect("a parent"))
        .expect("the parent exists");
    std::fs::write(&crash_directory_path, b"x").expect("plant a file where the directory would go");

    build_fixed_crash_report().write_crash_report(&crash_directory_path);

    assert_eq!(
        std::fs::read(&crash_directory_path).expect("the planted file is still there"),
        b"x"
    );

    let _ = std::fs::remove_file(&crash_directory_path);
}

#[test]
fn a_panic_writes_a_crash_report_naming_the_message_and_the_location() {
    let _panic_hook_test_guard = get_panic_hook_test_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let crash_directory_path = build_crash_directory_path("panic-message");
    let previous_panic_hook = panic::take_hook();
    panic::set_hook(Box::new(|_| {}));

    let panic_line_number;
    {
        let cleanup_guard = TerminalCleanupGuard::new();
        let _panic_hook_guard =
            install_panic_hook(&cleanup_guard, Some(crash_directory_path.clone()));
        panic_line_number = line!() + 1;
        let panic_result = panic::catch_unwind(AssertUnwindSafe(|| panic!("boom")));
        assert_eq!(
            panic_result.unwrap_err().downcast_ref::<&str>(),
            Some(&"boom")
        );
    }

    panic::set_hook(previous_panic_hook);

    let report_text = read_single_crash_report(&crash_directory_path);
    let (report_header, report_stack) = report_text
        .split_once("\nbacktrace:\n")
        .expect("the report ends with the stack");
    let report_header_lines: Vec<&str> = report_header.lines().collect();
    assert_eq!(report_header_lines.len(), 5, "{report_text}");
    assert_eq!(
        report_header_lines[0],
        format!("version: {}", env!("CARGO_PKG_VERSION"))
    );
    assert_eq!(
        report_header_lines[1],
        format!(
            "platform: {} {}",
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    );
    let timestamp_text = report_header_lines[2]
        .strip_prefix("timestamp: ")
        .expect("the third line is the timestamp");
    let _: u64 = timestamp_text
        .parse()
        .expect("the timestamp is whole seconds");
    assert!(
        crash_directory_path
            .join(format!("crash-{timestamp_text}.txt"))
            .is_file(),
        "the file is named by the timestamp line: {report_text}"
    );
    assert_eq!(report_header_lines[3], "message: boom");
    let panic_location_text = report_header_lines[4]
        .strip_prefix(&format!("location: {}:", file!()))
        .expect("the location names this test file");
    let (panic_line_text, panic_column_text) = panic_location_text
        .split_once(':')
        .expect("the location ends with line:column");
    assert_eq!(
        panic_line_text,
        panic_line_number.to_string(),
        "the line of the `panic!`"
    );
    let _: u32 = panic_column_text.parse().expect("the column is a number");
    assert!(
        !report_stack.trim().is_empty(),
        "the stack is not empty: {report_text}"
    );

    let _ = std::fs::remove_dir_all(&crash_directory_path);
}

#[test]
fn a_panic_with_no_message_writes_the_stand_in_text() {
    let _panic_hook_test_guard = get_panic_hook_test_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let crash_directory_path = build_crash_directory_path("panic-no-message");
    let previous_panic_hook = panic::take_hook();
    panic::set_hook(Box::new(|_| {}));

    {
        let cleanup_guard = TerminalCleanupGuard::new();
        let _panic_hook_guard =
            install_panic_hook(&cleanup_guard, Some(crash_directory_path.clone()));
        // A payload that is not a string carries no message to read.
        let panic_result = panic::catch_unwind(AssertUnwindSafe(|| panic::panic_any(42u32)));
        assert_eq!(panic_result.unwrap_err().downcast_ref::<u32>(), Some(&42));
    }

    panic::set_hook(previous_panic_hook);

    let report_text = read_single_crash_report(&crash_directory_path);
    assert!(
        report_text.contains("\nmessage: a panic with no message\n"),
        "{report_text}"
    );

    let _ = std::fs::remove_dir_all(&crash_directory_path);
}

// A `panic!` with format arguments carries a `String` payload; the message is
// written the same as a literal one.
#[test]
fn a_formatted_panic_message_is_written_in_full() {
    let _panic_hook_test_guard = get_panic_hook_test_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let crash_directory_path = build_crash_directory_path("panic-formatted");
    let previous_panic_hook = panic::take_hook();
    panic::set_hook(Box::new(|_| {}));

    {
        let cleanup_guard = TerminalCleanupGuard::new();
        let _panic_hook_guard =
            install_panic_hook(&cleanup_guard, Some(crash_directory_path.clone()));
        let pane_index = 7;
        let panic_result =
            panic::catch_unwind(AssertUnwindSafe(|| panic!("pane {pane_index} is gone")));
        assert_eq!(
            panic_result.unwrap_err().downcast_ref::<String>(),
            Some(&"pane 7 is gone".to_string())
        );
    }

    panic::set_hook(previous_panic_hook);

    let report_text = read_single_crash_report(&crash_directory_path);
    assert!(
        report_text.contains("\nmessage: pane 7 is gone\n"),
        "{report_text}"
    );

    let _ = std::fs::remove_dir_all(&crash_directory_path);
}

#[test]
fn a_panic_with_a_multi_line_message_keeps_every_line() {
    let _panic_hook_test_guard = get_panic_hook_test_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let crash_directory_path = build_crash_directory_path("panic-multi-line");
    let previous_panic_hook = panic::take_hook();
    panic::set_hook(Box::new(|_| {}));

    {
        let cleanup_guard = TerminalCleanupGuard::new();
        let _panic_hook_guard =
            install_panic_hook(&cleanup_guard, Some(crash_directory_path.clone()));
        let panic_result = panic::catch_unwind(AssertUnwindSafe(|| {
            panic!("first line\nsecond line\nthird")
        }));
        assert_eq!(
            panic_result.unwrap_err().downcast_ref::<&str>(),
            Some(&"first line\nsecond line\nthird")
        );
    }

    panic::set_hook(previous_panic_hook);

    let report_text = read_single_crash_report(&crash_directory_path);
    assert!(
        report_text.contains("\nmessage: first line\nsecond line\nthird\nlocation: "),
        "every line of the message is kept, and `location` follows it: {report_text}"
    );

    let _ = std::fs::remove_dir_all(&crash_directory_path);
}

#[test]
fn a_panic_with_no_crash_directory_writes_no_file() {
    let _panic_hook_test_guard = get_panic_hook_test_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let crash_directory_path = build_crash_directory_path("no-crash-directory");
    std::fs::create_dir_all(&crash_directory_path).expect("create the directory to watch");
    let previous_panic_hook = panic::take_hook();
    panic::set_hook(Box::new(|_| {}));

    {
        let cleanup_guard = TerminalCleanupGuard::new();
        let _panic_hook_guard = install_panic_hook(&cleanup_guard, None);
        let panic_result = panic::catch_unwind(AssertUnwindSafe(|| panic!("boom")));
        assert_eq!(
            panic_result.unwrap_err().downcast_ref::<&str>(),
            Some(&"boom")
        );
    }

    panic::set_hook(previous_panic_hook);

    assert_eq!(
        std::fs::read_dir(&crash_directory_path)
            .expect("the directory is readable")
            .count(),
        0,
        "no crash directory was named, so no report is written"
    );

    let _ = std::fs::remove_dir_all(&crash_directory_path);
}

#[test]
fn the_cleanup_hooks_run_before_the_crash_report_is_written() {
    let _panic_hook_test_guard = get_panic_hook_test_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let crash_directory_path = build_crash_directory_path("hooks-first");
    let report_presence_seen_by_hook = Arc::new(Mutex::new(None));
    let previous_panic_hook = panic::take_hook();
    panic::set_hook(Box::new(|_| {}));

    {
        let cleanup_guard = TerminalCleanupGuard::new();
        let watched_crash_directory_path = crash_directory_path.clone();
        let report_presence_slot = Arc::clone(&report_presence_seen_by_hook);
        cleanup_guard.register_cleanup(Box::new(move || {
            let report_already_written = std::fs::read_dir(&watched_crash_directory_path)
                .is_ok_and(|directory_entries| directory_entries.count() > 0);
            *report_presence_slot.lock().unwrap() = Some(report_already_written);
        }));
        let _panic_hook_guard =
            install_panic_hook(&cleanup_guard, Some(crash_directory_path.clone()));
        let panic_result = panic::catch_unwind(AssertUnwindSafe(|| panic!("boom")));
        assert_eq!(
            panic_result.unwrap_err().downcast_ref::<&str>(),
            Some(&"boom")
        );
    }

    panic::set_hook(previous_panic_hook);

    assert_eq!(
        *report_presence_seen_by_hook.lock().unwrap(),
        Some(false),
        "the terminal is restored before any report is written"
    );
    assert!(
        read_single_crash_report(&crash_directory_path).contains("\nmessage: boom\n"),
        "the report is written once the hooks are done"
    );

    let _ = std::fs::remove_dir_all(&crash_directory_path);
}

#[test]
fn a_crash_report_that_cannot_be_written_still_restores_the_terminal() {
    let _panic_hook_test_guard = get_panic_hook_test_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // The crash directory's path is a file, so writing the report fails.
    let base_directory_path = build_crash_directory_path("write-fails");
    std::fs::create_dir_all(&base_directory_path).expect("create the base directory");
    let blocked_path = base_directory_path.join("not-a-directory");
    std::fs::write(&blocked_path, b"i am a file").expect("write the blocking file");
    let cleanup_count = Arc::new(AtomicUsize::new(0));
    let previous_panic_hook = panic::take_hook();
    panic::set_hook(Box::new(|_| {}));

    {
        let cleanup_guard = TerminalCleanupGuard::new();
        let cleanup_count_for_hook = Arc::clone(&cleanup_count);
        cleanup_guard.register_cleanup(Box::new(move || {
            cleanup_count_for_hook.fetch_add(1, Ordering::SeqCst);
        }));
        let _panic_hook_guard = install_panic_hook(&cleanup_guard, Some(blocked_path.clone()));
        let panic_result = panic::catch_unwind(AssertUnwindSafe(|| panic!("boom")));
        assert_eq!(
            panic_result.unwrap_err().downcast_ref::<&str>(),
            Some(&"boom")
        );
    }

    panic::set_hook(previous_panic_hook);

    assert_eq!(
        cleanup_count.load(Ordering::SeqCst),
        1,
        "the cleanup hook runs even though the report cannot be written"
    );
    assert_eq!(
        std::fs::read_dir(&base_directory_path)
            .expect("the base directory is readable")
            .count(),
        1,
        "nothing but the blocking file is there"
    );

    let _ = std::fs::remove_dir_all(&base_directory_path);
}

#[cfg(unix)]
#[test]
fn a_crash_report_is_readable_only_by_its_owner() {
    // The report holds the panic message, the place and the stack; no other
    // local user reads it.
    use std::os::unix::fs::PermissionsExt as _;

    let crash_directory_path = build_crash_directory_path("owner-only");
    build_fixed_crash_report().write_crash_report(&crash_directory_path);

    let get_path_mode = |inspected_path: &Path| {
        std::fs::metadata(inspected_path)
            .expect("the path exists")
            .permissions()
            .mode()
            & 0o777
    };
    assert_eq!(
        get_path_mode(&crash_directory_path),
        0o700,
        "the crash directory is owner-only"
    );
    assert_eq!(
        get_path_mode(&crash_directory_path.join("crash-1700000000.txt")),
        0o600,
        "the crash report is owner-only"
    );

    let _ = std::fs::remove_dir_all(&crash_directory_path);
}
