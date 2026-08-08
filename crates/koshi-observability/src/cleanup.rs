//! `cleanup` domain — terminal restoration that survives panics.
//!
//! Koshi puts the terminal into raw mode and the alternate screen while it runs.
//! If the process exits without undoing that — including an unwinding panic — the
//! user is left with a corrupted shell. [`cleanup::TerminalCleanupGuard`] guarantees the
//! undo: callers register cleanup hooks, and the hooks run exactly once on
//! whichever comes first — the guard being dropped, or a panic, if
//! [`cleanup::install_panic_hook`] armed one.
//!
//! An armed panic hook also writes a crash report to the directory the caller
//! names, as `crash-<timestamp>.txt`, after the cleanup hooks run.
//!
//! This module ships only the mechanism. The concrete hooks — disabling raw mode
//! and leaving the alternate screen via `crossterm` — are registered by the
//! runtime when it actually enters those modes, so this crate takes no terminal
//! dependency. Hooks are plain [`FnOnce`] closures here.

use std::panic::{self, AssertUnwindSafe, PanicHookInfo};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

/// A one-shot terminal-cleanup action. Boxed and `Send` so it can be held in the
/// shared registry and run from either the dropping thread or the panic hook.
pub type CleanupHook = Box<dyn FnOnce() + Send>;

/// The hook registry, shared between the guard and any installed panic hook.
type Registry = Arc<Mutex<Vec<CleanupHook>>>;

/// A shareable panic hook, so the installed chained hook and the
/// [`PanicHookGuard`] that restores it can both hold the prior hook.
type SharedPanicHook = Arc<dyn Fn(&PanicHookInfo<'_>) + Sync + Send + 'static>;

/// One panic as the crash file records it: the time, the message, the place,
/// and the stack. [`CrashReport::capture`] reads it on the panicking thread,
/// where the payload and the stack are still reachable; another thread writes
/// it.
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
    /// Read one panic into an owned report.
    fn capture(info: &PanicHookInfo<'_>) -> CrashReport {
        CrashReport {
            timestamp: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |since| since.as_secs()),
            message: info
                .payload_as_str()
                .unwrap_or("a panic with no message")
                .to_string(),
            location: info
                .location()
                .map_or_else(|| "unknown".to_string(), ToString::to_string),
            backtrace: std::backtrace::Backtrace::force_capture().to_string(),
        }
    }

    /// The file's text: one `field: value` line per fact, the stack last.
    fn text(&self) -> String {
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

    /// Write the report to `<dir>/crash-<timestamp>.txt`. A directory that
    /// cannot be created and a write that fails both leave no file and report
    /// nothing.
    fn write(&self, dir: &Path) {
        if koshi_paths::ensure_dir(dir).is_err() {
            return;
        }
        let _ = std::fs::write(
            dir.join(format!("crash-{}.txt", self.timestamp)),
            self.text(),
        );
    }
}

/// Runs its registered [cleanup hooks](CleanupHook) exactly once — on drop, or on
/// panic if [`install_panic_hook`] was called with this guard. Hooks run in the
/// order they were registered.
///
/// The guard owns the registry; [`install_panic_hook`] shares it with the process
/// panic hook. Whichever path fires first drains and runs the hooks, so the other
/// finds an empty registry and does nothing — a hook never runs twice.
pub struct TerminalCleanupGuard {
    hooks: Registry,
}

impl TerminalCleanupGuard {
    /// Create a guard with no hooks registered yet.
    pub fn new() -> Self {
        Self {
            hooks: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Register a hook to run at cleanup. Hooks run in registration order.
    pub fn register_cleanup(&self, hook: CleanupHook) {
        self.hooks
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
        run_hooks(&self.hooks);
    }
}

/// Restores the panic hook that was installed before [`install_panic_hook`], on
/// drop. Holding it for the terminal session's lifetime keeps the chained hook
/// active; dropping it unchains cleanup so a later session does not stack inert
/// wrappers on the process-global hook.
///
/// The process panic hook is a single global slot. Keep one guard active at a
/// time. Drop the guards in reverse install order (LIFO) — the natural lifetime
/// of a nested scope. `Drop` restores the captured hook whenever the dropping
/// thread is not itself panicking.
#[must_use = "dropping the returned guard immediately restores the previous panic hook"]
pub struct PanicHookGuard {
    previous: Option<SharedPanicHook>,
}

impl Drop for PanicHookGuard {
    fn drop(&mut self) {
        // `set_hook` itself panics when called from a panicking thread, which
        // would turn the in-flight panic into a destructor abort. A drop while
        // unwinding returns here instead, leaving the chained hook installed.
        //
        // If that unwind is later caught and the process runs on, the chained
        // hook stays installed until a later, non-panicking drop restores the
        // previous one. Until then it runs no cleanup hook — the registry is
        // already drained — and still writes a crash report for each further
        // panic when a crash directory was named.
        if std::thread::panicking() {
            return;
        }
        if let Some(previous) = self.previous.take() {
            panic::set_hook(Box::new(move |info| previous(info)));
        }
    }
}

/// Chain a panic hook that runs `guard`'s cleanup hooks and writes a crash
/// report before the previously installed hook. Terminal restoration happens
/// first, so by the time the panic message prints, the terminal is already
/// back on its normal screen with raw mode disabled.
///
/// The crash report goes to `crash_dir` as `crash-<timestamp>.txt`, after the
/// cleanup hooks. `None` reads no panic and writes no file.
///
/// The panic hook shares the guard's registry, so a panic and a later drop draw
/// from the same set: whichever runs first drains it, and the other is a no-op.
///
/// Returns a [`PanicHookGuard`] that restores the previous hook when dropped.
pub fn install_panic_hook(
    guard: &TerminalCleanupGuard,
    crash_dir: Option<PathBuf>,
) -> PanicHookGuard {
    let hooks = Arc::clone(&guard.hooks);
    let previous: SharedPanicHook = Arc::from(panic::take_hook());
    let chained = Arc::clone(&previous);
    panic::set_hook(Box::new(move |info| {
        let report = crash_dir
            .as_ref()
            .map(|dir| (dir.clone(), CrashReport::capture(info)));
        restore_then_report(&hooks, report);
        chained(info);
    }));
    PanicHookGuard {
        previous: Some(previous),
    }
}

/// Run the cleanup hooks, then write the crash report, both on one fresh
/// thread ([`on_fresh_thread`]). The thread starts every time, with or
/// without a registered hook. The report is written after the hooks, so the
/// terminal is back on its normal screen first, and inside
/// [`catch_unwind`](panic::catch_unwind), the same as a hook.
///
/// `report` pairs the directory that takes the file with the report itself.
/// It is absent when no crash directory was named.
///
/// A hook that panics on the spawned thread re-enters this function through
/// the chained hook. It finds the registry empty, runs no hook, and writes its
/// own report. The first thread writes over that file when both reports fall
/// in the same whole second.
fn restore_then_report(hooks: &Registry, report: Option<(PathBuf, CrashReport)>) {
    let drained = drain_hooks(hooks);
    on_fresh_thread(move || {
        run_each(drained);
        if let Some((dir, report)) = report {
            let _ = panic::catch_unwind(AssertUnwindSafe(|| report.write(&dir)));
        }
    });
}

/// Run `work` on a fresh thread and wait for it to finish. A panic raised
/// while Rust is executing a panic hook aborts the process before any
/// `catch_unwind` landing pad, so hook work stays off the panicking thread.
///
/// Spawning may fail under resource exhaustion mid-panic; then `work` is
/// dropped unrun, and the terminal may be left dirty.
fn on_fresh_thread(work: impl FnOnce() + Send + 'static) {
    if let Ok(handle) = std::thread::Builder::new().spawn(work) {
        let _ = handle.join();
    }
}

/// Take every registered hook out of the registry, leaving it empty. The lock
/// is held only for the swap, so a hook may register another hook while it
/// runs and a slow hook never holds the registry. A poisoned lock is
/// recovered: cleanup must still run when another thread died.
fn drain_hooks(hooks: &Registry) -> Vec<CleanupHook> {
    let mut guard = hooks
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    std::mem::take(&mut *guard)
}

/// Run the cleanup hooks for a dropped guard: drain the registry and run every
/// hook in registration order. Each hook runs inside
/// [`catch_unwind`](panic::catch_unwind), so a hook that panics does not stop
/// the hooks after it.
///
/// A drop that happens while the thread is already unwinding runs the hooks on
/// a fresh thread ([`on_fresh_thread`]).
///
/// The installed panic hook uses [`restore_then_report`].
fn run_hooks(hooks: &Registry) {
    let drained = drain_hooks(hooks);
    if drained.is_empty() {
        return;
    }

    if std::thread::panicking() {
        on_fresh_thread(move || run_each(drained));
    } else {
        run_each(drained);
    }
}

/// Run each hook in order, isolating a panicking hook so the rest still run.
fn run_each(hooks: Vec<CleanupHook>) {
    for hook in hooks {
        let _ = panic::catch_unwind(AssertUnwindSafe(hook));
    }
}

#[cfg(test)]
mod tests;
