//! `koshi-beta` — the beta-feature gate and the `#[beta_feature]` attribute.
//!
//! The attribute marks an entry point that runs only when beta features are
//! allowed. A gated function stays ordinary code: it takes no gate argument and
//! holds no gate field. The attribute is the whole gate. Taking a feature out
//! of beta deletes one attribute line per site.
//!
//! `koshi.kdl`'s top-level `allow-beta-features` decides whether a gated entry
//! point runs. Koshi reads that setting once at startup and stores it here with
//! [`set_allowed`]. The body is always compiled in. The gate reads the stored
//! flag where the body would start: at the call for an ordinary function, at
//! the first poll for an `async fn`.
//!
//! [`koshi-macro`](koshi_macro) compiles the attribute. This crate re-exports
//! it. A crate that gates a function depends on this crate alone.

pub use koshi_macro::beta_feature;

use std::sync::atomic::{AtomicBool, Ordering};

static ALLOWED: AtomicBool = AtomicBool::new(false);

/// Stores `allowed` as the process-wide answer [`allowed`] returns. Each call
/// replaces the stored value. Koshi calls this once per process, with
/// `allow-beta-features` from the loaded config.
pub fn set_allowed(allowed: bool) {
    ALLOWED.store(allowed, Ordering::Relaxed);
}

/// Returns whether beta-gated entry points may run: the last value given to
/// [`set_allowed`], or `false` when no call has happened in this process.
#[must_use]
pub fn allowed() -> bool {
    ALLOWED.load(Ordering::Relaxed)
}

/// Emits one `tracing` event at `WARN` level on every call. The event carries
/// a `function` field holding `function`. Its message says `function` did
/// nothing and names the `koshi.kdl` line that lets it run. `function` goes
/// into the message unchanged: no escaping, no truncation.
///
/// The code `#[beta_feature]` generates calls this on the first blocked call
/// of each gated function only, and passes the module path and the identifier
/// joined by `::`, such as `session::attach`.
///
/// `log_blocked("attach")` writes the message:
/// ``` text
/// `attach` is a beta feature and did nothing; add a top-level `allow-beta-features #true` line to koshi.kdl to run it
/// ```
pub fn log_blocked(function: &str) {
    tracing::warn!(
        function,
        "`{function}` is a beta feature and did nothing; add a top-level \
         `allow-beta-features #true` line to koshi.kdl to run it"
    );
}

#[cfg(test)]
mod tests;
