//! The process-wide beta-feature gate.
//!
//! `koshi.kdl`'s top-level `allow-beta-features` decides whether entry points
//! carrying `#[beta_feature]` run their body. The knob is read once at
//! startup and stored here, because a gated function keeps the signature it
//! will have once the feature is stable: it takes no gate argument and holds
//! no gate field, so the attribute has nothing to read but a process global.
//! Taking a feature out of beta is deleting the attribute line.
//!
//! The attribute itself lives in the `koshi-beta` proc-macro crate and
//! expands to calls on [`allowed`] and [`log_blocked`].

use std::sync::atomic::{AtomicBool, Ordering};

static ALLOWED: AtomicBool = AtomicBool::new(false);

/// Records whether beta-gated entry points may run. Called once per process,
/// with `allow-beta-features` from the loaded config.
pub fn set_allowed(allowed: bool) {
    ALLOWED.store(allowed, Ordering::Relaxed);
}

/// Whether beta-gated entry points may run. `false` until [`set_allowed`]
/// says otherwise, so a process that never loads a config runs nothing beta.
#[must_use]
pub fn allowed() -> bool {
    ALLOWED.load(Ordering::Relaxed)
}

/// Logs that the entry point named `function` did not run, naming the knob
/// that would let it. Callers log this at most once per gated function.
pub fn log_blocked(function: &str) {
    tracing::warn!(
        function,
        "beta feature is off, so `{function}` did nothing; add a top-level \
         `allow-beta-features #true` line to koshi.kdl to run it"
    );
}

#[cfg(test)]
mod tests;
