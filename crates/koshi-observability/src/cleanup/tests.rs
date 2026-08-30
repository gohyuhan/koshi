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
        let _panic_guard = install_panic_hook(&guard, None);

        let result = panic::catch_unwind(AssertUnwindSafe(|| panic!("boom")));
        assert_eq!(result.unwrap_err().downcast_ref::<&str>(), Some(&"boom"));
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
// hook*. The hooks run on a fresh thread, so reaching the assertions at all
// proves the hooks ran off the panic path. The hook after the panicking one
// must still run.
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
        let _panic_guard = install_panic_hook(&guard, None);

        let result = panic::catch_unwind(AssertUnwindSafe(|| panic!("boom")));
        assert_eq!(result.unwrap_err().downcast_ref::<&str>(), Some(&"boom"));
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

#[test]
fn a_default_guard_runs_its_hooks_on_drop() {
    let ran = Arc::new(AtomicUsize::new(0));
    {
        let guard = TerminalCleanupGuard::default();
        let counter = Arc::clone(&ran);
        guard.register_cleanup(Box::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        }));
    }

    assert_eq!(ran.load(Ordering::SeqCst), 1);
}

// A thread that dies while it holds the registry poisons that lock. The
// terminal must still be restored: registering and draining both recover the
// poisoned lock instead of panicking.
#[test]
fn cleanup_still_runs_after_a_thread_died_holding_the_registry() {
    let _serial = panic_hook_test_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let before = Arc::new(AtomicUsize::new(0));
    let after = Arc::new(AtomicUsize::new(0));

    let guard = TerminalCleanupGuard::new();
    let before_counter = Arc::clone(&before);
    guard.register_cleanup(Box::new(move || {
        before_counter.fetch_add(1, Ordering::SeqCst);
    }));

    // Silence the default hook so the deliberate panic below stays quiet.
    let saved = panic::take_hook();
    panic::set_hook(Box::new(|_| {}));
    let hooks = Arc::clone(&guard.hooks);
    let died = std::thread::spawn(move || {
        let _held = hooks.lock().expect("the registry is not poisoned yet");
        panic!("the thread holding the registry died");
    });
    let payload = died.join().expect_err("the spawned thread must have died");
    assert_eq!(
        payload.downcast_ref::<&str>(),
        Some(&"the thread holding the registry died")
    );
    panic::set_hook(saved);

    // Registering into the poisoned registry still works.
    let after_counter = Arc::clone(&after);
    guard.register_cleanup(Box::new(move || {
        after_counter.fetch_add(1, Ordering::SeqCst);
    }));

    drop(guard);

    assert_eq!(
        before.load(Ordering::SeqCst),
        1,
        "a hook registered before the poisoning must still run"
    );
    assert_eq!(
        after.load(Ordering::SeqCst),
        1,
        "a hook registered after the poisoning must still run"
    );
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
        let _panic_guard = install_panic_hook(&guard, None);

        let result = panic::catch_unwind(AssertUnwindSafe(|| panic!("boom")));
        assert_eq!(result.unwrap_err().downcast_ref::<&str>(), Some(&"boom"));
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

    let panic_guard = install_panic_hook(&guard, None);
    drop(panic_guard); // restores the silent no-op hook set above

    let result = panic::catch_unwind(AssertUnwindSafe(|| panic!("boom")));
    assert_eq!(result.unwrap_err().downcast_ref::<&str>(), Some(&"boom"));
    assert_eq!(
        counter.load(Ordering::SeqCst),
        0,
        "cleanup must not run: the panic hook was unchained before this panic fired"
    );

    panic::set_hook(saved);
    drop(guard);
}

// Two guards installed one inside the other. Dropping the inner one puts the
// outer chained hook back; dropping the outer one puts the original hook back.
#[test]
fn nested_panic_hook_guards_dropped_last_in_first_out_restore_each_previous_hook() {
    let _serial = panic_hook_test_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let original_calls = Arc::new(AtomicUsize::new(0));
    let outer_cleanups = Arc::new(AtomicUsize::new(0));
    let inner_cleanups = Arc::new(AtomicUsize::new(0));
    let saved = panic::take_hook();
    let original = Arc::clone(&original_calls);
    panic::set_hook(Box::new(move |_| {
        original.fetch_add(1, Ordering::SeqCst);
    }));

    let outer = TerminalCleanupGuard::new();
    let inner = TerminalCleanupGuard::new();
    let counter = Arc::clone(&outer_cleanups);
    outer.register_cleanup(Box::new(move || {
        counter.fetch_add(1, Ordering::SeqCst);
    }));
    let counter = Arc::clone(&inner_cleanups);
    inner.register_cleanup(Box::new(move || {
        counter.fetch_add(1, Ordering::SeqCst);
    }));
    let outer_hook = install_panic_hook(&outer, None);
    let inner_hook = install_panic_hook(&inner, None);

    drop(inner_hook);
    let result = panic::catch_unwind(AssertUnwindSafe(|| panic!("boom")));
    assert_eq!(result.unwrap_err().downcast_ref::<&str>(), Some(&"boom"));
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
    assert_eq!(original_calls.load(Ordering::SeqCst), 1);

    // A fresh outer hook shows whether outer is still chained after its guard drops.
    let counter = Arc::clone(&outer_cleanups);
    outer.register_cleanup(Box::new(move || {
        counter.fetch_add(1, Ordering::SeqCst);
    }));
    drop(outer_hook);
    let result = panic::catch_unwind(AssertUnwindSafe(|| panic!("again")));
    assert_eq!(result.unwrap_err().downcast_ref::<&str>(), Some(&"again"));
    assert_eq!(
        outer_cleanups.load(Ordering::SeqCst),
        1,
        "outer is unchained"
    );
    assert_eq!(
        original_calls.load(Ordering::SeqCst),
        2,
        "the original hook is back in place"
    );

    panic::set_hook(saved);
    drop(inner);
    drop(outer);
}

// The previously installed hook runs after the cleanup hooks: when it fires,
// the cleanup hook has already counted.
#[test]
fn the_previous_panic_hook_runs_after_the_cleanup_hooks() {
    let _serial = panic_hook_test_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let restored = Arc::new(AtomicUsize::new(0));
    let seen_by_previous = Arc::new(Mutex::new(None));
    let saved = panic::take_hook();
    let watched = Arc::clone(&restored);
    let seen = Arc::clone(&seen_by_previous);
    panic::set_hook(Box::new(move |_| {
        *seen.lock().unwrap() = Some(watched.load(Ordering::SeqCst));
    }));

    {
        let guard = TerminalCleanupGuard::new();
        let counter = Arc::clone(&restored);
        guard.register_cleanup(Box::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        }));
        let _panic_guard = install_panic_hook(&guard, None);
        let result = panic::catch_unwind(AssertUnwindSafe(|| panic!("boom")));
        assert_eq!(result.unwrap_err().downcast_ref::<&str>(), Some(&"boom"));
    }

    // Read the value out and put the original hook back before asserting: the
    // installed hook takes this same lock, so a failing assertion that still
    // held it would block instead of reporting.
    let seen = *seen_by_previous.lock().unwrap();
    panic::set_hook(saved);

    assert_eq!(
        seen,
        Some(1),
        "the previous hook ran once, after the cleanup hook"
    );
}

// A `PanicHookGuard` dropped while its thread is unwinding restores nothing:
// the chained hook stays installed, and the next panic still drains the
// registry.
#[test]
fn a_panic_hook_guard_dropped_while_unwinding_leaves_the_chained_hook_installed() {
    let _serial = panic_hook_test_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let ran = Arc::new(AtomicUsize::new(0));
    let saved = panic::take_hook();
    panic::set_hook(Box::new(|_| {}));

    let guard = TerminalCleanupGuard::new();
    let panic_guard = install_panic_hook(&guard, None);
    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        let _dropped_while_unwinding = panic_guard;
        panic!("boom")
    }));
    assert_eq!(result.unwrap_err().downcast_ref::<&str>(), Some(&"boom"));

    let counter = Arc::clone(&ran);
    guard.register_cleanup(Box::new(move || {
        counter.fetch_add(1, Ordering::SeqCst);
    }));
    let result = panic::catch_unwind(AssertUnwindSafe(|| panic!("again")));
    assert_eq!(result.unwrap_err().downcast_ref::<&str>(), Some(&"again"));
    assert_eq!(
        ran.load(Ordering::SeqCst),
        1,
        "the chained hook still drains the registry"
    );

    panic::set_hook(saved);
    drop(guard);
}

// A guard dropped while its thread unwinds still runs its hooks, and runs them
// on a fresh thread rather than on the unwinding one.
#[test]
fn a_guard_dropped_while_unwinding_runs_its_hooks_on_another_thread() {
    let _serial = panic_hook_test_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let hook_thread = Arc::new(Mutex::new(None));
    let saved = panic::take_hook();
    panic::set_hook(Box::new(|_| {}));

    let seen = Arc::clone(&hook_thread);
    let result = panic::catch_unwind(AssertUnwindSafe(move || {
        let guard = TerminalCleanupGuard::new();
        guard.register_cleanup(Box::new(move || {
            *seen.lock().unwrap() = Some(std::thread::current().id());
        }));
        panic!("boom")
    }));
    assert_eq!(result.unwrap_err().downcast_ref::<&str>(), Some(&"boom"));

    panic::set_hook(saved);

    let ran_on = hook_thread
        .lock()
        .unwrap()
        .expect("the hook ran while the thread unwound");
    assert_ne!(
        ran_on,
        std::thread::current().id(),
        "the hook runs off the unwinding thread"
    );
}

// A hook that registers another hook while it runs does not block on the
// registry. The new hook is not part of the drain that is running; the next
// drain runs it.
#[test]
fn a_hook_registered_by_a_running_hook_runs_on_the_next_drain() {
    let _serial = panic_hook_test_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let ran = Arc::new(AtomicUsize::new(0));
    let saved = panic::take_hook();
    panic::set_hook(Box::new(|_| {}));

    {
        let guard = TerminalCleanupGuard::new();
        let registry = Arc::clone(&guard.hooks);
        let counter = Arc::clone(&ran);
        guard.register_cleanup(Box::new(move || {
            registry
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(Box::new(move || {
                    counter.fetch_add(1, Ordering::SeqCst);
                }));
        }));
        let _panic_guard = install_panic_hook(&guard, None);

        let result = panic::catch_unwind(AssertUnwindSafe(|| panic!("boom")));
        assert_eq!(result.unwrap_err().downcast_ref::<&str>(), Some(&"boom"));
        assert_eq!(
            ran.load(Ordering::SeqCst),
            0,
            "the hook registered during the drain is not part of it"
        );
    } // drop drains again: the hook the first hook registered runs now

    assert_eq!(ran.load(Ordering::SeqCst), 1);

    panic::set_hook(saved);
}

// --- The crash report ---

/// A fresh directory for one crash-report test, removed if a previous run
/// left it behind. The tag keeps parallel tests from sharing a directory.
fn crash_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("koshi-crash-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// A report with every field fixed, so its file name and text are known.
fn fixed_report() -> CrashReport {
    CrashReport {
        timestamp: 1_700_000_000,
        message: "boom".to_string(),
        location: "src/main.rs:10:5".to_string(),
        backtrace: "frame one\nframe two".to_string(),
    }
}

/// The text of the one crash report under `dir`. Panics when the directory
/// holds anything other than exactly one file.
fn only_crash_report(dir: &Path) -> String {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .expect("the crash directory exists")
        .map(|entry| entry.expect("the directory entry is readable").path())
        .collect();
    paths.sort();
    assert_eq!(paths.len(), 1, "expected one crash report, found {paths:?}");
    std::fs::read_to_string(&paths[0]).expect("the crash report is readable")
}

#[test]
fn a_crash_report_writes_a_file_named_by_its_timestamp_holding_every_fact() {
    let dir = crash_dir("every-fact");

    fixed_report().write(&dir);

    let text = std::fs::read_to_string(dir.join("crash-1700000000.txt"))
        .expect("the report is written under its timestamp");
    assert_eq!(
        text,
        format!(
            "version: {}\nplatform: {} {}\ntimestamp: 1700000000\nmessage: boom\nlocation: src/main.rs:10:5\nbacktrace:\nframe one\nframe two\n",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_second_report_with_the_same_timestamp_replaces_the_first() {
    let dir = crash_dir("same-second");
    let mut later = fixed_report();
    later.message = "later".to_string();

    fixed_report().write(&dir);
    later.write(&dir);

    assert_eq!(
        only_crash_report(&dir),
        format!(
            "version: {}\nplatform: {} {}\ntimestamp: 1700000000\nmessage: later\nlocation: src/main.rs:10:5\nbacktrace:\nframe one\nframe two\n",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_crash_report_creates_the_directories_leading_to_its_file() {
    let dir = crash_dir("missing-parents").join("nested").join("deeper");

    fixed_report().write(&dir);

    assert!(
        dir.join("crash-1700000000.txt").is_file(),
        "the report is written under the directories it created"
    );

    let _ = std::fs::remove_dir_all(crash_dir("missing-parents"));
}

#[test]
fn a_crash_report_whose_directory_cannot_be_created_writes_nothing() {
    // The crash directory's path is already a file, so creating it fails.
    let base = crash_dir("dir-is-a-file");
    std::fs::create_dir_all(&base).expect("create the base directory");
    let blocked = base.join("not-a-directory");
    std::fs::write(&blocked, b"i am a file").expect("write the blocking file");

    fixed_report().write(&blocked);

    assert_eq!(
        std::fs::read_to_string(&blocked).expect("the blocking file is readable"),
        "i am a file",
        "the blocking file is left untouched"
    );
    assert_eq!(
        std::fs::read_dir(&base)
            .expect("the base directory is readable")
            .count(),
        1,
        "nothing else was written"
    );

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn a_crash_report_whose_file_path_is_a_directory_writes_nothing() {
    // The directory is writable, but the report's own file name is taken by
    // a directory, so the write itself fails.
    let dir = crash_dir("file-is-a-directory");
    let taken = dir.join("crash-1700000000.txt");
    std::fs::create_dir_all(&taken).expect("create the blocking directory");

    fixed_report().write(&dir);

    assert!(taken.is_dir(), "the blocking directory is left in place");
    assert_eq!(
        std::fs::read_dir(&taken)
            .expect("the blocking directory is readable")
            .count(),
        0,
        "nothing was written into it"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// Only Unix can make a directory read-only through `std`. On Windows a failing
// write is covered by
// `a_crash_report_whose_file_path_is_a_directory_writes_nothing`.
#[cfg(unix)]
#[test]
fn a_crash_report_into_a_read_only_directory_writes_nothing() {
    use std::os::unix::fs::PermissionsExt;

    let dir = crash_dir("read-only");
    std::fs::create_dir_all(&dir).expect("create the crash directory");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500))
        .expect("make the directory read-only");

    // A user who overrides directory permissions (root) can still write
    // here, so probe first and only assert once the permission holds.
    let probe = dir.join("probe");
    let enforced = std::fs::write(&probe, b"x").is_err();
    let _ = std::fs::remove_file(&probe);

    if enforced {
        fixed_report().write(&dir);
        assert_eq!(
            std::fs::read_dir(&dir)
                .expect("the directory is readable")
                .count(),
            0,
            "a read-only directory takes no report"
        );
    }

    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
        .expect("restore the directory so it can be removed");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_panic_writes_a_crash_report_naming_the_message_and_the_location() {
    let _serial = panic_hook_test_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let dir = crash_dir("panic-message");
    let saved = panic::take_hook();
    panic::set_hook(Box::new(|_| {}));

    let panic_line;
    {
        let guard = TerminalCleanupGuard::new();
        let _panic_guard = install_panic_hook(&guard, Some(dir.clone()));
        panic_line = line!() + 1;
        let result = panic::catch_unwind(AssertUnwindSafe(|| panic!("boom")));
        assert_eq!(result.unwrap_err().downcast_ref::<&str>(), Some(&"boom"));
    }

    panic::set_hook(saved);

    let text = only_crash_report(&dir);
    let (header, stack) = text
        .split_once("\nbacktrace:\n")
        .expect("the report ends with the stack");
    let lines: Vec<&str> = header.lines().collect();
    assert_eq!(lines.len(), 5, "{text}");
    assert_eq!(lines[0], format!("version: {}", env!("CARGO_PKG_VERSION")));
    assert_eq!(
        lines[1],
        format!(
            "platform: {} {}",
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    );
    let timestamp = lines[2]
        .strip_prefix("timestamp: ")
        .expect("the third line is the timestamp");
    let _: u64 = timestamp.parse().expect("the timestamp is whole seconds");
    assert!(
        dir.join(format!("crash-{timestamp}.txt")).is_file(),
        "the file is named by the timestamp line: {text}"
    );
    assert_eq!(lines[3], "message: boom");
    let position = lines[4]
        .strip_prefix(&format!("location: {}:", file!()))
        .expect("the location names this test file");
    let (line, column) = position
        .split_once(':')
        .expect("the location ends with line:column");
    assert_eq!(line, panic_line.to_string(), "the line of the `panic!`");
    let _: u32 = column.parse().expect("the column is a number");
    assert!(!stack.trim().is_empty(), "the stack is not empty: {text}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_panic_with_no_message_writes_the_stand_in_text() {
    let _serial = panic_hook_test_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let dir = crash_dir("panic-no-message");
    let saved = panic::take_hook();
    panic::set_hook(Box::new(|_| {}));

    {
        let guard = TerminalCleanupGuard::new();
        let _panic_guard = install_panic_hook(&guard, Some(dir.clone()));
        // A payload that is not a string carries no message to read.
        let result = panic::catch_unwind(AssertUnwindSafe(|| panic::panic_any(42u32)));
        assert_eq!(result.unwrap_err().downcast_ref::<u32>(), Some(&42));
    }

    panic::set_hook(saved);

    let text = only_crash_report(&dir);
    assert!(
        text.contains("\nmessage: a panic with no message\n"),
        "{text}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// A `panic!` with format arguments carries a `String` payload; the message is
// written the same as a literal one.
#[test]
fn a_formatted_panic_message_is_written_in_full() {
    let _serial = panic_hook_test_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let dir = crash_dir("panic-formatted");
    let saved = panic::take_hook();
    panic::set_hook(Box::new(|_| {}));

    {
        let guard = TerminalCleanupGuard::new();
        let _panic_guard = install_panic_hook(&guard, Some(dir.clone()));
        let pane = 7;
        let result = panic::catch_unwind(AssertUnwindSafe(|| panic!("pane {pane} is gone")));
        assert_eq!(
            result.unwrap_err().downcast_ref::<String>(),
            Some(&"pane 7 is gone".to_string())
        );
    }

    panic::set_hook(saved);

    let text = only_crash_report(&dir);
    assert!(text.contains("\nmessage: pane 7 is gone\n"), "{text}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_panic_with_a_multi_line_message_keeps_every_line() {
    let _serial = panic_hook_test_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let dir = crash_dir("panic-multi-line");
    let saved = panic::take_hook();
    panic::set_hook(Box::new(|_| {}));

    {
        let guard = TerminalCleanupGuard::new();
        let _panic_guard = install_panic_hook(&guard, Some(dir.clone()));
        let result = panic::catch_unwind(AssertUnwindSafe(|| {
            panic!("first line\nsecond line\nthird")
        }));
        assert_eq!(
            result.unwrap_err().downcast_ref::<&str>(),
            Some(&"first line\nsecond line\nthird")
        );
    }

    panic::set_hook(saved);

    let text = only_crash_report(&dir);
    assert!(
        text.contains("\nmessage: first line\nsecond line\nthird\nlocation: "),
        "every line of the message is kept, and `location` follows it: {text}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_panic_with_no_crash_directory_writes_no_file() {
    let _serial = panic_hook_test_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let dir = crash_dir("no-crash-directory");
    std::fs::create_dir_all(&dir).expect("create the directory to watch");
    let saved = panic::take_hook();
    panic::set_hook(Box::new(|_| {}));

    {
        let guard = TerminalCleanupGuard::new();
        let _panic_guard = install_panic_hook(&guard, None);
        let result = panic::catch_unwind(AssertUnwindSafe(|| panic!("boom")));
        assert_eq!(result.unwrap_err().downcast_ref::<&str>(), Some(&"boom"));
    }

    panic::set_hook(saved);

    assert_eq!(
        std::fs::read_dir(&dir)
            .expect("the directory is readable")
            .count(),
        0,
        "no crash directory was named, so no report is written"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_cleanup_hooks_run_before_the_crash_report_is_written() {
    let _serial = panic_hook_test_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let dir = crash_dir("hooks-first");
    let report_seen_by_hook = Arc::new(Mutex::new(None));
    let saved = panic::take_hook();
    panic::set_hook(Box::new(|_| {}));

    {
        let guard = TerminalCleanupGuard::new();
        let watched = dir.clone();
        let seen = Arc::clone(&report_seen_by_hook);
        guard.register_cleanup(Box::new(move || {
            let already_there =
                std::fs::read_dir(&watched).is_ok_and(|entries| entries.count() > 0);
            *seen.lock().unwrap() = Some(already_there);
        }));
        let _panic_guard = install_panic_hook(&guard, Some(dir.clone()));
        let result = panic::catch_unwind(AssertUnwindSafe(|| panic!("boom")));
        assert_eq!(result.unwrap_err().downcast_ref::<&str>(), Some(&"boom"));
    }

    panic::set_hook(saved);

    assert_eq!(
        *report_seen_by_hook.lock().unwrap(),
        Some(false),
        "the terminal is restored before any report is written"
    );
    assert!(
        only_crash_report(&dir).contains("\nmessage: boom\n"),
        "the report is written once the hooks are done"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_crash_report_that_cannot_be_written_still_restores_the_terminal() {
    let _serial = panic_hook_test_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // The crash directory's path is a file, so writing the report fails.
    let base = crash_dir("write-fails");
    std::fs::create_dir_all(&base).expect("create the base directory");
    let blocked = base.join("not-a-directory");
    std::fs::write(&blocked, b"i am a file").expect("write the blocking file");
    let restored = Arc::new(AtomicUsize::new(0));
    let saved = panic::take_hook();
    panic::set_hook(Box::new(|_| {}));

    {
        let guard = TerminalCleanupGuard::new();
        let counter = Arc::clone(&restored);
        guard.register_cleanup(Box::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        }));
        let _panic_guard = install_panic_hook(&guard, Some(blocked.clone()));
        let result = panic::catch_unwind(AssertUnwindSafe(|| panic!("boom")));
        assert_eq!(result.unwrap_err().downcast_ref::<&str>(), Some(&"boom"));
    }

    panic::set_hook(saved);

    assert_eq!(
        restored.load(Ordering::SeqCst),
        1,
        "the cleanup hook runs even though the report cannot be written"
    );
    assert_eq!(
        std::fs::read_dir(&base)
            .expect("the base directory is readable")
            .count(),
        1,
        "nothing but the blocking file is there"
    );

    let _ = std::fs::remove_dir_all(&base);
}
