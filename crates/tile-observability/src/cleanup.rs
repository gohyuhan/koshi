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

use std::panic;
use std::sync::{Arc, Mutex};

/// A one-shot terminal-cleanup action. Boxed and `Send` so it can be held in the
/// shared registry and run from either the dropping thread or the panic hook.
pub type CleanupHook = Box<dyn FnOnce() + Send>;

/// The hook registry, shared between the guard and any installed panic hook.
type Registry = Arc<Mutex<Vec<CleanupHook>>>;

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

/// Chain a panic hook that runs `guard`'s cleanup hooks before the previously
/// installed hook. Terminal restoration happens first so the panic message and
/// any crash report land on a sane screen rather than the alternate buffer.
///
/// The panic hook shares the guard's registry, so a panic and a later drop draw
/// from the same set: whichever runs first drains it, and the other is a no-op.
pub fn install_panic_hook(guard: &TerminalCleanupGuard) {
    let hooks = Arc::clone(&guard.hooks);
    let previous = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        run_hooks(&hooks);
        previous(info);
    }));
}

/// Drain the registry and run every hook in registration order. The lock is
/// released before any hook runs, both so a hook may itself register (without
/// deadlocking) and so a slow or panicking hook never holds the registry. A
/// poisoned lock is recovered: cleanup must still run when another thread died.
fn run_hooks(hooks: &Registry) {
    let drained: Vec<CleanupHook> = {
        let mut guard = hooks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        std::mem::take(&mut *guard)
    };
    for hook in drained {
        hook();
    }
}

#[cfg(test)]
mod tests;
