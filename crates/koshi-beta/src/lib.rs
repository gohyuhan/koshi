//! `koshi-beta` — the beta-feature gate and the `#[beta_feature]` attribute.
//!
//! Marks an entry point as not stable enough to run for everyone yet. The
//! function is written as ordinary, finished code; the attribute is the whole
//! gate, so taking the feature out of beta is deleting one line per site.
//!
//! `koshi.kdl`'s top-level `allow-beta-features` decides whether a gated entry
//! point runs. The knob is read once at startup and stored here, because a
//! gated function keeps the signature it will have once the feature is stable:
//! it takes no gate argument and holds no gate field, so the attribute has
//! nothing to read but a process global. The body is always compiled in, and
//! the flag is read on every call, so one build serves both answers.
//!
//! The attribute is compiled by [`koshi-macro`](koshi_macro), which is separate
//! because a `proc-macro` crate may export nothing but macros. It is re-exported
//! here, so a crate that gates a function depends on this crate alone and the
//! generated `koshi_beta::allowed` call always resolves.

pub use koshi_macro::beta_feature;

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

/// What a blocked entry point has to say for itself: that `function` did not
/// run, and the knob that would let it.
///
/// A gated function whose `otherwise` is an error carries this as the error's
/// text, so the user reads the same sentence the log would have carried.
#[must_use]
pub fn blocked_message(function: &str) -> String {
    format!(
        "`{function}` is a beta feature and did nothing; add a top-level \
         `allow-beta-features #true` line to koshi.kdl to run it"
    )
}

/// Logs that the entry point named `function` did not run, naming the knob
/// that would let it. Callers log this at most once per gated function.
pub fn log_blocked(function: &str) {
    tracing::warn!(function, "{}", blocked_message(function));
}

#[cfg(test)]
mod tests;
