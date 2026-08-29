//! Tests for `TerminalCleanupGuard` and the panic hook it arms: hooks run in
//! registration order on drop, a panic runs them exactly once through the
//! installed hook, and a panicking hook neither aborts the process nor stops
//! the hooks after it.
//!
//! Then the crash report: what the file is named and what it holds, that the
//! recent-event fields exclude payloads, that a locked ring does not block the
//! hook, that cleanup runs before writing, and that write failures leave no file.

use super::*;
use koshi_core::event::{Event, PaneEnterPressed, SubmittedLinePayload};
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};
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
    assert!(died.join().is_err(), "the spawned thread must have died");
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

    let panic_guard = install_panic_hook(&guard, None);
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
        recent_events: Some(Vec::new()),
    }
}

/// The paths of the crash reports under `dir`, excluding the report lock.
fn crash_report_paths(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .expect("the crash directory exists")
        .map(|entry| entry.expect("the directory entry is readable").path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("crash-") && name.ends_with(".txt"))
        })
        .collect()
}

/// The text of the one crash report under `dir`.
fn only_crash_report(dir: &Path) -> String {
    let mut paths = crash_report_paths(dir);
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
            "version: {}\nplatform: {} {}\ntimestamp: 1700000000\nmessage: boom\nlocation: src/main.rs:10:5\nrecent_events:\nbacktrace:\nframe one\nframe two\n",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn crash_reports_with_the_same_timestamp_get_distinct_files() {
    let dir = crash_dir("same-timestamp");
    let mut first = fixed_report();
    first.message = "first".to_string();
    first.write(&dir);
    let mut second = fixed_report();
    second.message = "second".to_string();
    second.write(&dir);

    let mut paths = crash_report_paths(&dir);
    paths.sort();
    assert_eq!(paths.len(), 2);
    let contents: Vec<String> = paths
        .iter()
        .map(|path| std::fs::read_to_string(path).expect("the report is readable"))
        .collect();
    assert!(contents.iter().any(|text| text.contains("message: first")));
    assert!(contents.iter().any(|text| text.contains("message: second")));

    for _ in 0..9 {
        fixed_report().write(&dir);
    }
    let count = crash_report_paths(&dir).len();
    assert_eq!(count, MAX_CRASH_REPORTS);

    let mut twelfth = fixed_report();
    twelfth.message = "twelfth".to_string();
    twelfth.write(&dir);
    let contents = crash_report_paths(&dir)
        .iter()
        .map(|path| std::fs::read_to_string(path).expect("the report is readable"))
        .collect::<Vec<_>>();
    assert_eq!(contents.len(), MAX_CRASH_REPORTS);
    assert!(
        contents
            .iter()
            .any(|text| text.contains("message: twelfth")),
        "the newest same-time report remains after retention"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn crash_report_does_not_reuse_plain_name_after_a_legacy_collision() {
    let dir = crash_dir("legacy-collision");
    std::fs::create_dir_all(&dir).expect("create the crash directory");
    std::fs::write(dir.join("crash-1700000000-42-0.txt"), "legacy\n")
        .expect("write the legacy report");

    fixed_report().write(&dir);

    assert!(!dir.join("crash-1700000000.txt").exists());
    assert!(dir.join("crash-1700000000-1.txt").is_file());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn crash_report_retention_treats_the_plain_name_as_oldest() {
    let dir = crash_dir("legacy-order");
    std::fs::create_dir_all(&dir).expect("create the crash directory");
    std::fs::write(dir.join("crash-1700000000.txt"), "plain\n").expect("write the plain report");
    std::fs::write(dir.join("crash-1700000000-42-0.txt"), "legacy\n")
        .expect("write the legacy report");
    for sequence in 1..=9 {
        let path = dir.join(format!("crash-1700000000-{sequence}.txt"));
        std::fs::write(path, format!("message: report {sequence}\n"))
            .expect("write the sequenced report");
    }

    retain_crash_reports(&dir);

    assert_eq!(crash_report_paths(&dir).len(), MAX_CRASH_REPORTS);
    assert!(!dir.join("crash-1700000000.txt").exists());
    assert!(dir.join("crash-1700000000-42-0.txt").is_file());
    assert!(dir.join("crash-1700000000-9.txt").is_file());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn crash_report_retention_keeps_a_newer_legacy_collision() {
    let dir = crash_dir("legacy-publication-order");
    std::fs::create_dir_all(&dir).expect("create the crash directory");
    let set_modified = |path: &Path, seconds: u64| {
        std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("open the report for timestamp setup")
            .set_modified(UNIX_EPOCH + std::time::Duration::from_secs(seconds))
            .expect("set the report timestamp");
    };
    for sequence in 1..=10 {
        let path = dir.join(format!("crash-1700000000-{sequence}.txt"));
        std::fs::write(&path, format!("message: report {sequence}\n"))
            .expect("write the sequenced report");
        set_modified(&path, sequence);
    }
    let legacy = dir.join("crash-1700000000-42-0.txt");
    std::fs::write(&legacy, "legacy\n").expect("write the legacy report");
    set_modified(&legacy, 11);

    retain_crash_reports(&dir);

    assert_eq!(crash_report_paths(&dir).len(), MAX_CRASH_REPORTS);
    assert!(!dir.join("crash-1700000000-1.txt").exists());
    assert!(legacy.is_file());
    assert!(dir.join("crash-1700000000-10.txt").is_file());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn crash_report_retention_keeps_legacy_when_modification_times_tie() {
    let dir = crash_dir("legacy-equal-modification-time");
    std::fs::create_dir_all(&dir).expect("create the crash directory");
    let set_modified = |path: &Path| {
        std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("open the report for timestamp setup")
            .set_modified(UNIX_EPOCH)
            .expect("set the report timestamp");
    };
    for sequence in 1..=10 {
        let path = dir.join(format!("crash-1700000000-{sequence}.txt"));
        std::fs::write(&path, format!("message: report {sequence}\n"))
            .expect("write the sequenced report");
        set_modified(&path);
    }
    let legacy = dir.join("crash-1700000000-42-0.txt");
    std::fs::write(&legacy, "legacy\n").expect("write the legacy report");
    set_modified(&legacy);

    retain_crash_reports(&dir);

    assert_eq!(crash_report_paths(&dir).len(), MAX_CRASH_REPORTS);
    assert!(!dir.join("crash-1700000000-1.txt").exists());
    assert!(legacy.is_file());
    assert!(dir.join("crash-1700000000-10.txt").is_file());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn crash_reports_with_publication_sequences_retain_the_newest_files() {
    let dir = crash_dir("publication-sequence");
    std::fs::create_dir_all(&dir).expect("create the crash directory");
    for sequence in 1..=11 {
        let path = dir.join(format!("crash-1700000000-{sequence}.txt"));
        std::fs::write(path, format!("message: report {sequence}\n"))
            .expect("write the sequenced report");
    }

    retain_crash_reports(&dir);

    assert_eq!(crash_report_paths(&dir).len(), MAX_CRASH_REPORTS);
    assert!(!dir.join("crash-1700000000-1.txt").exists());
    assert!(dir.join("crash-1700000000-11.txt").is_file());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_crash_report_keeps_only_the_newest_ten_files() {
    let dir = crash_dir("retention");

    for timestamp in 0..=10_u64 {
        let mut report = fixed_report();
        report.timestamp = timestamp;
        report.write(&dir);
    }

    let mut paths = crash_report_paths(&dir);
    paths.sort();
    assert_eq!(paths.len(), 10);
    assert!(!dir.join("crash-0.txt").exists());
    assert!(dir.join("crash-10.txt").exists());

    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn a_crash_report_directory_is_private() {
    use std::os::unix::fs::PermissionsExt;

    let dir = crash_dir("directory-mode");
    fixed_report().write(&dir);

    let mode = std::fs::metadata(&dir)
        .expect("the crash directory exists")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o700);

    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn a_crash_report_file_is_owner_only() {
    use std::os::unix::fs::PermissionsExt;

    let dir = crash_dir("file-mode");
    fixed_report().write(&dir);

    let mode = std::fs::metadata(dir.join("crash-1700000000.txt"))
        .expect("the report exists")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);

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
    // The directory exists before its private ACL is applied. The existing
    // child must remain readable when the report name is taken by a directory.
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

#[cfg(unix)]
#[test]
fn a_crash_report_repairs_a_same_user_directory_to_private_mode() {
    use std::os::unix::fs::PermissionsExt;

    let dir = crash_dir("repair-directory-mode");
    std::fs::create_dir_all(&dir).expect("create the crash directory");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755))
        .expect("make the directory wider than private");

    fixed_report().write(&dir);

    let mode = std::fs::metadata(&dir)
        .expect("the crash directory exists")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o700);
    assert!(
        dir.join("crash-1700000000.txt").is_file(),
        "the report is written after the directory is repaired"
    );

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

    {
        let guard = TerminalCleanupGuard::new();
        let _panic_guard = install_panic_hook(&guard, Some(dir.clone()));
        let result = panic::catch_unwind(AssertUnwindSafe(|| panic!("boom")));
        assert!(result.is_err());
    }

    panic::set_hook(saved);

    let text = only_crash_report(&dir);
    assert!(
        text.starts_with(&format!(
            "version: {}\nplatform: {} {}\ntimestamp: ",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            std::env::consts::ARCH
        )),
        "{text}"
    );
    assert!(text.contains("\nmessage: boom\n"), "{text}");
    assert!(
        text.contains(&format!("\nlocation: {}:", file!())),
        "the location names this test file: {text}"
    );
    let (_, stack) = text
        .split_once("\nbacktrace:\n")
        .expect("the report ends with the stack");
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
        assert!(result.is_err());
    }

    panic::set_hook(saved);

    let text = only_crash_report(&dir);
    assert!(
        text.contains("\nmessage: a panic with no message\n"),
        "{text}"
    );

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
        assert!(result.is_err());
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
fn a_panic_writes_recent_event_names_and_ids_without_payload_text() {
    let _serial = panic_hook_test_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let _recent_serial = crate::logging::recent_events::lock_for_test();
    let dir = crash_dir("panic-recent-events");
    let session_id = SessionId::new();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let secret = "crash-event-payload-secret";
    crate::logging::recent_events::record(&Event::PaneEnterPressed(PaneEnterPressed {
        pane_id,
        tab_id,
        session_id,
        client_id,
        line: SubmittedLinePayload::SafePublic(secret.to_string()),
        timestamp: SystemTime::now(),
    }));
    let saved = panic::take_hook();
    panic::set_hook(Box::new(|_| {}));

    {
        let guard = TerminalCleanupGuard::new();
        let _panic_guard = install_panic_hook(&guard, Some(dir.clone()));
        let result = panic::catch_unwind(AssertUnwindSafe(|| panic!("boom")));
        assert!(result.is_err());
    }

    panic::set_hook(saved);

    let text = only_crash_report(&dir);
    assert!(text.contains("event: PaneEnterPressed"), "{text}");
    assert!(text.contains(&session_id.to_string()), "{text}");
    assert!(text.contains(&client_id.to_string()), "{text}");
    assert!(text.contains(&tab_id.to_string()), "{text}");
    assert!(text.contains(&pane_id.to_string()), "{text}");
    assert!(!text.contains(secret), "event payload leaked: {text}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_panic_while_recent_events_are_locked_still_writes_the_crash_report() {
    let _serial = panic_hook_test_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let _recent_serial = crate::logging::recent_events::lock_for_test();
    let dir = crash_dir("panic-recent-events-locked");
    let saved = panic::take_hook();
    panic::set_hook(Box::new(|_| {}));

    {
        let guard = TerminalCleanupGuard::new();
        let _panic_guard = install_panic_hook(&guard, Some(dir.clone()));
        let result = panic::catch_unwind(AssertUnwindSafe(|| {
            crate::logging::recent_events::with_lock_for_test(|| panic!("boom"));
        }));
        assert!(result.is_err(), "the deliberate panic must unwind");
    }

    panic::set_hook(saved);

    let text = only_crash_report(&dir);
    let (header, _) = text
        .split_once("\nbacktrace:\n")
        .expect("the report ends with the stack");
    assert!(
        !header.contains("recent_events:"),
        "the locked ring must omit its unavailable section: {text}"
    );
    assert!(
        header.contains("\nlocation: "),
        "the locked ring must not block the panic hook: {text}"
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
        assert!(result.is_err());
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
        assert!(result.is_err());
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
        assert!(result.is_err());
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
