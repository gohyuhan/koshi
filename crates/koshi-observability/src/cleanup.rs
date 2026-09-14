//! `cleanup` domain — terminal restoration that survives panics.
//!
//! Koshi puts the terminal into raw mode and the alternate screen while it
//! runs. [`cleanup::TerminalCleanupGuard`] undoes that on exit: callers
//! register cleanup hooks, and the hooks run exactly once on whichever comes
//! first — the guard being dropped, or a panic, if
//! [`cleanup::install_panic_hook`] armed one.
//!
//! An armed panic hook also writes a crash report to the directory the caller
//! names, as `crash-<timestamp>.txt`, after the cleanup hooks run.
//!
//! Hooks are plain [`FnOnce`] closures. The runtime registers the ones that
//! disable raw mode and leave the alternate screen; this crate takes no
//! terminal dependency.

use std::panic::{self, AssertUnwindSafe, PanicHookInfo};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

/// A one-shot terminal-cleanup action. It runs at most once: on the thread
/// that drops the guard, or on a fresh thread when the drop happens while
/// unwinding or when the panic hook fires.
pub type CleanupHook = Box<dyn FnOnce() + Send>;

/// The cleanup-hook registry, shared between the guard and any installed panic hook.
type CleanupHookRegistry = Arc<Mutex<Vec<CleanupHook>>>;

/// The panic hook that was installed before [`install_panic_hook`], held by
/// both the chained hook and the [`PanicHookGuard`] that restores it.
type SharedPanicHook = Arc<dyn Fn(&PanicHookInfo<'_>) + Sync + Send + 'static>;

/// One panic as the crash file records it: the time, the message, the place,
/// and the stack. [`CrashReport::capture_crash_report`] reads it on the panicking
/// thread; [`CrashReport::write_crash_report`] runs on another thread.
struct CrashReport {
    /// Whole seconds since the Unix epoch. Also names the file: `1754640000`
    /// results in `crash-1754640000.txt`.
    timestamp: u64,
    /// The panic message.
    message: String,
    /// `file:line:column` of the panic.
    location: String,
    /// The panicking thread's stack.
    backtrace: String,
}

impl CrashReport {
    /// Read one panic into an owned report. A payload that is not a string
    /// reads as `a panic with no message`, a panic with no location as
    /// `unknown`, and a clock before the Unix epoch as timestamp `0`.
    fn capture_crash_report(panic_info: &PanicHookInfo<'_>) -> CrashReport {
        CrashReport {
            timestamp: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |elapsed_since_epoch| elapsed_since_epoch.as_secs()),
            message: panic_info
                .payload_as_str()
                .unwrap_or("a panic with no message")
                .to_string(),
            location: panic_info
                .location()
                .map_or_else(|| "unknown".to_string(), ToString::to_string),
            backtrace: std::backtrace::Backtrace::force_capture().to_string(),
        }
    }

    /// The file's text: one `field: value` line per fact, the stack last.
    fn render_crash_report_text(&self) -> String {
        format!(
            "version: {}\nplatform: {} {}\ntimestamp: {}\nmessage: {}\nlocation: {}\nbacktrace:\n{}\n",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            std::env::consts::ARCH,
            self.timestamp,
            self.message,
            self.location,
            self.backtrace,
        )
    }

    /// Write the report to `<directory_path>/crash-<timestamp>.txt`.
    ///
    /// On Unix the directory is created and verified as `0700` and a new file
    /// is created as `0600`. A directory setup error returns without opening
    /// the file. Open and write errors are ignored; an existing file may be
    /// truncated before a write error occurs. For timestamp `1700000000`, the
    /// file is `<directory_path>/crash-1700000000.txt`.
    fn write_crash_report(&self, directory_path: &Path) {
        if koshi_paths::ensure_private_directory(directory_path).is_err() {
            return;
        }
        let crash_file_path = directory_path.join(format!("crash-{}.txt", self.timestamp));
        #[cfg(unix)]
        {
            use std::io::Write as _;
            use std::os::unix::fs::OpenOptionsExt as _;

            let crash_file_open_result = std::fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .mode(0o600)
                .open(&crash_file_path);
            if let Ok(mut crash_file) = crash_file_open_result {
                let _ = crash_file.write_all(self.render_crash_report_text().as_bytes());
            }
        }
        #[cfg(not(unix))]
        {
            let _ = std::fs::write(&crash_file_path, self.render_crash_report_text());
        }
    }
}

/// Runs its registered [cleanup hooks](CleanupHook) exactly once — on drop, or on
/// panic if [`install_panic_hook`] was called with this guard. Hooks run in the
/// order they were registered.
///
/// The guard owns the registry; [`install_panic_hook`] shares it with the process
/// panic hook. Whichever path fires first drains and runs the hooks; the other
/// finds an empty registry and does nothing. A hook never runs twice.
pub struct TerminalCleanupGuard {
    cleanup_hooks: CleanupHookRegistry,
}

impl TerminalCleanupGuard {
    /// Create a guard with no hooks registered yet.
    pub fn new() -> Self {
        Self {
            cleanup_hooks: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Register a hook to run at cleanup. Hooks run in registration order.
    pub fn register_cleanup(&self, hook: CleanupHook) {
        self.cleanup_hooks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(hook);
    }
}

impl Default for TerminalCleanupGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for TerminalCleanupGuard {
    fn drop(&mut self) {
        run_cleanup_hooks(&self.cleanup_hooks);
    }
}

/// Restores the panic hook that was installed before [`install_panic_hook`], on
/// drop. While it is alive, the chained hook stays installed.
///
/// The process panic hook is a single global slot. Keep one guard active at a
/// time. Drop the guards in reverse install order (LIFO) — the natural lifetime
/// of a nested scope. `Drop` restores the captured hook only when the dropping
/// thread is not itself panicking.
#[must_use = "dropping the returned guard immediately restores the previous panic hook"]
pub struct PanicHookGuard {
    previous_panic_hook: SharedPanicHook,
}

impl Drop for PanicHookGuard {
    fn drop(&mut self) {
        // `set_hook` panics on a panicking thread. A drop while unwinding
        // returns here and leaves the chained hook installed until the next
        // `set_hook`. That hook then runs no cleanup hook (the registry is
        // already drained) and still writes a crash report for each further
        // panic when a crash directory was named.
        if std::thread::panicking() {
            return;
        }
        let previous_panic_hook = Arc::clone(&self.previous_panic_hook);
        panic::set_hook(Box::new(move |panic_info| previous_panic_hook(panic_info)));
    }
}

/// Chain a panic hook that runs `cleanup_guard`'s cleanup hooks, writes a crash
/// report, and then calls the previously installed hook. Under the default
/// hook, the panic message prints after the cleanup hooks have run.
///
/// The crash report goes to `crash_directory_path` as `crash-<timestamp>.txt`, after the
/// cleanup hooks. `None` reads no panic and writes no file.
///
/// The panic hook shares the cleanup guard's registry: whichever of a panic and a
/// cleanup-guard drop runs first drains it, and the other is a no-op.
///
/// Returns a [`PanicHookGuard`] that restores the previous hook when dropped.
pub fn install_panic_hook(
    cleanup_guard: &TerminalCleanupGuard,
    crash_directory_path: Option<PathBuf>,
) -> PanicHookGuard {
    let cleanup_hooks = Arc::clone(&cleanup_guard.cleanup_hooks);
    let previous_panic_hook: SharedPanicHook = Arc::from(panic::take_hook());
    let chained_panic_hook = Arc::clone(&previous_panic_hook);
    panic::set_hook(Box::new(move |panic_info| {
        let crash_report_option = crash_directory_path
            .as_ref()
            .map(|named_crash_directory_path| {
                (
                    named_crash_directory_path.clone(),
                    CrashReport::capture_crash_report(panic_info),
                )
            });
        run_cleanup_then_write_crash_report(&cleanup_hooks, crash_report_option);
        chained_panic_hook(panic_info);
    }));
    PanicHookGuard {
        previous_panic_hook,
    }
}

/// Run the cleanup hooks, then write the crash report, both on one fresh
/// thread ([`run_on_fresh_thread`]). The thread starts every time, with or
/// without a registered hook. The crash report is written after the hooks, inside
/// [`catch_unwind`](panic::catch_unwind), the same as a hook.
///
/// `crash_report` pairs the directory that takes the file with the report itself.
/// It is absent when no crash directory was named.
///
/// A hook that panics on the spawned thread re-enters this function through
/// the chained hook. It finds the registry empty, runs no hook, and writes its
/// own report. The first thread writes over that file when both reports fall
/// in the same whole second.
fn run_cleanup_then_write_crash_report(
    cleanup_hooks: &CleanupHookRegistry,
    crash_report: Option<(PathBuf, CrashReport)>,
) {
    let drained_cleanup_hooks = drain_cleanup_hooks(cleanup_hooks);
    run_on_fresh_thread(move || {
        run_cleanup_hooks_in_order(drained_cleanup_hooks);
        if let Some((crash_directory_path, crash_report)) = crash_report {
            let _ = panic::catch_unwind(AssertUnwindSafe(|| {
                crash_report.write_crash_report(&crash_directory_path)
            }));
        }
    });
}

/// Run `thread_task` on a fresh thread and wait for it to finish. A panic inside
/// `thread_task` unwinds on that thread; a panic on a thread that is running a panic
/// hook aborts the process before any `catch_unwind` landing pad.
///
/// If the thread cannot be spawned, `thread_task` is dropped without running.
fn run_on_fresh_thread(thread_task: impl FnOnce() + Send + 'static) {
    if let Ok(thread_handle) = std::thread::Builder::new().spawn(thread_task) {
        let _ = thread_handle.join();
    }
}

/// Take every registered hook out of the registry, leaving it empty. The lock
/// is held only for the swap; a hook may register another hook while it runs.
/// A poisoned lock is recovered.
fn drain_cleanup_hooks(cleanup_hooks: &CleanupHookRegistry) -> Vec<CleanupHook> {
    let mut registry_guard = cleanup_hooks
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    std::mem::take(&mut *registry_guard)
}

/// Run the cleanup hooks for a dropped guard: drain the registry and run every
/// hook in registration order. Each hook runs inside
/// [`catch_unwind`](panic::catch_unwind); a hook that panics does not stop
/// the hooks after it.
///
/// A drop that happens while the thread is already unwinding runs the hooks on
/// a fresh thread ([`run_on_fresh_thread`]).
///
/// The installed panic hook uses [`run_cleanup_then_write_crash_report`].
fn run_cleanup_hooks(cleanup_hooks: &CleanupHookRegistry) {
    let drained_cleanup_hooks = drain_cleanup_hooks(cleanup_hooks);
    if drained_cleanup_hooks.is_empty() {
        return;
    }

    if std::thread::panicking() {
        run_on_fresh_thread(move || run_cleanup_hooks_in_order(drained_cleanup_hooks));
    } else {
        run_cleanup_hooks_in_order(drained_cleanup_hooks);
    }
}

/// Run each hook in order inside [`catch_unwind`](panic::catch_unwind); a
/// hook that panics does not stop the hooks after it.
fn run_cleanup_hooks_in_order(cleanup_hooks: Vec<CleanupHook>) {
    for cleanup_hook in cleanup_hooks {
        let _ = panic::catch_unwind(AssertUnwindSafe(cleanup_hook));
    }
}

#[cfg(test)]
mod tests;
