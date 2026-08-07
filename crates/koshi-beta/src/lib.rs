//! `koshi-beta` — the beta-feature gate and the `#[beta_feature]` attribute.
//!
//! The attribute marks an entry point as not stable enough to run for everyone
//! yet. A gated function stays ordinary, finished code: it takes no gate
//! argument and holds no gate field. The attribute is the whole gate, so taking
//! a feature out of beta is deleting one line per site.
//!
//! `koshi.kdl`'s top-level `allow-beta-features` decides whether a gated entry
//! point runs. Koshi reads that setting once at startup and stores it here. The
//! body is always compiled in. The flag is read on every call, so one build
//! serves both answers.
//!
//! [`koshi-macro`](koshi_macro) compiles the attribute. This crate re-exports
//! it, so a crate that gates a function depends on this crate alone.

pub use koshi_macro::beta_feature;

use std::sync::atomic::{AtomicBool, Ordering};

static ALLOWED: AtomicBool = AtomicBool::new(false);

/// Records whether beta-gated entry points may run. Koshi calls this once per
/// process, with `allow-beta-features` from the loaded config.
pub fn set_allowed(allowed: bool) {
    ALLOWED.store(allowed, Ordering::Relaxed);
}

/// Returns whether beta-gated entry points may run. The value is `false` until
/// [`set_allowed`] changes it, so a process that never loads a config runs
/// nothing beta.
#[must_use]
pub fn allowed() -> bool {
    ALLOWED.load(Ordering::Relaxed)
}

/// Returns the message for a blocked entry point: `function` did not run, and
/// the setting that would let it run.
///
/// A gated function whose `otherwise` is an error carries this as the error
/// text.
#[must_use]
pub fn blocked_message(function: &str) -> String {
    format!(
        "`{function}` is a beta feature and did nothing; add a top-level \
         `allow-beta-features #true` line to koshi.kdl to run it"
    )
}

/// Logs a warning that the entry point named `function` did not run, and names
/// the setting that would let it run. Callers log this at most once per gated
/// function.
pub fn log_blocked(function: &str) {
    tracing::warn!(function, "{}", blocked_message(function));
}

#[cfg(test)]
mod tests;
