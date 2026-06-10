//! `cleanup` domain — terminal restoration that survives panics.
//!
//! Tile puts the terminal into raw mode and the alternate screen while it runs.
//! If the process exits without undoing that — including an unwinding panic — the
//! user is left with a corrupted shell. [`TerminalCleanupGuard`] guarantees the
//! undo: callers register cleanup hooks, and the hooks run exactly once on
//! whichever comes first — the guard being dropped, or a panic, if
//! [`install_panic_hook`] armed one.
//!
//! This module ships only the mechanism. The concrete hooks — disabling raw mode
//! and leaving the alternate screen via `crossterm` — are registered by the
//! runtime when it actually enters those modes, so this crate takes no terminal
//! dependency. Hooks are plain [`FnOnce`] closures here.

use std::panic::{self, AssertUnwindSafe, PanicHookInfo};
use std::sync::{Arc, Mutex};

/// A one-shot terminal-cleanup action. Boxed and `Send` so it can be held in the
/// shared registry and run from either the dropping thread or the panic hook.
pub type CleanupHook = Box<dyn FnOnce() + Send>;

/// The hook registry, shared between the guard and any installed panic hook.
type Registry = Arc<Mutex<Vec<CleanupHook>>>;

/// A shareable panic hook, so the installed chained hook and the
/// [`PanicHookGuard`] that restores it can both hold the prior hook.
type SharedPanicHook = Arc<dyn Fn(&PanicHookInfo<'_>) + Sync + Send + 'static>;

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
/// The process panic hook is a single global slot, so only one of these guards
/// should be active at a time and they must be dropped in reverse install order
/// (LIFO) — the natural lifetime of a nested scope. `Drop` restores the captured
/// hook unconditionally, so dropping out of order would overwrite a hook some
/// other component installed afterward.
#[must_use = "dropping the returned guard immediately restores the previous panic hook"]
pub struct PanicHookGuard {
    previous: Option<SharedPanicHook>,
}

impl Drop for PanicHookGuard {
    fn drop(&mut self) {
        // `set_hook` itself panics if called from a panicking thread, which would
        // turn the in-flight panic into a destructor abort — the exact opposite
        // of this module's goal. When we are unwinding, the chained hook has
        // already run; leave it installed rather than abort to restore it.
        //
        // If that unwind is later caught and the process continues, the chained
        // hook stays installed as an inert wrapper (its registry is already
        // drained). This intentionally prefers preserving the original panic over
        // restoring the previous hook here, since `set_hook` is illegal mid-panic.
        if std::thread::panicking() {
            return;
        }
        if let Some(previous) = self.previous.take() {
            panic::set_hook(Box::new(move |info| previous(info)));
        }
    }
}

/// Chain a panic hook that runs `guard`'s cleanup hooks before the previously
/// installed hook. Terminal restoration happens first so the panic message and
/// any crash report land on a sane screen rather than the alternate buffer.
///
/// The panic hook shares the guard's registry, so a panic and a later drop draw
/// from the same set: whichever runs first drains it, and the other is a no-op.
///
/// Returns a [`PanicHookGuard`] that restores the previous hook when dropped.
pub fn install_panic_hook(guard: &TerminalCleanupGuard) -> PanicHookGuard {
    let hooks = Arc::clone(&guard.hooks);
    let previous: SharedPanicHook = Arc::from(panic::take_hook());
    let chained = Arc::clone(&previous);
    panic::set_hook(Box::new(move |info| {
        run_hooks(&hooks);
        chained(info);
    }));
    PanicHookGuard {
        previous: Some(previous),
    }
}

/// Drain the registry and run every hook in registration order. The lock is
/// released before any hook runs, both so a hook may itself register (without
/// deadlocking) and so a slow hook never holds the registry. A poisoned lock is
/// recovered: cleanup must still run when another thread died.
///
/// A hook that panics must neither abort the process nor skip the hooks after it.
/// How that is achieved depends on where we are running:
///
/// - Off the panic path (a normal drop), each hook runs inside
///   [`catch_unwind`](panic::catch_unwind), which isolates its panic.
/// - From the installed panic hook, `catch_unwind` is not enough: a panic raised
///   while Rust is executing a panic hook aborts the process immediately, before
///   any `catch_unwind` landing pad. So the hooks run on a fresh thread instead,
///   where each hook's panic is an ordinary first-level panic that `catch_unwind`
///   contains. (A panicking hook there re-enters this function via the chained
///   hook, but finds the registry already drained, so it cannot recurse.)
fn run_hooks(hooks: &Registry) {
    let drained: Vec<CleanupHook> = {
        let mut guard = hooks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        std::mem::take(&mut *guard)
    };
    if drained.is_empty() {
        return;
    }

    if std::thread::panicking() {
        // Spawning may fail under resource exhaustion mid-panic; then the hooks
        // are dropped unrun (the terminal may be left dirty) rather than risk the
        // abort that running them inline here would invite.
        if let Ok(handle) = std::thread::Builder::new().spawn(move || run_each(drained)) {
            let _ = handle.join();
        }
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
