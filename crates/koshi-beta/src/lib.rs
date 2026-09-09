//! `koshi-beta` provides the process-wide beta-feature gate and re-exports the
//! `#[beta_feature]` attribute.
//!
//! The attribute keeps a gated function's signature and body ordinary. The
//! gate reads the value stored by [`set_allowed`] at the start of the body: at
//! the call for an ordinary function and at the first poll for an `async fn`.
//! The function body stays compiled whether the gate is open or closed.
//!
//! The top-level `allow-beta-features` value in `koshi.kdl` supplies the stored
//! value. [`koshi-macro`](koshi_macro) expands the attribute, and this crate
//! provides the generated calls. A gated crate depends on this crate.

pub use koshi_macro::beta_feature;

use std::sync::atomic::{AtomicBool, Ordering};

static ALLOWED: AtomicBool = AtomicBool::new(false);

/// Sets the process-wide value returned by [`allowed`]. Each call replaces it.
/// Koshi passes the loaded `allow-beta-features` value once during startup.
pub fn set_allowed(allowed: bool) {
    ALLOWED.store(allowed, Ordering::Relaxed);
}

/// Returns whether beta-gated entry points may run. It returns the last value
/// passed to [`set_allowed`], or `false` when no call has happened in the
/// process.
#[must_use]
pub fn allowed() -> bool {
    ALLOWED.load(Ordering::Relaxed)
}

/// Emits one `WARN`-level `tracing` event per call.
///
/// The event has a `function` field containing `function`. Its message says
/// that `function` did nothing and names the `koshi.kdl` line that enables it.
/// The message contains `function` unchanged, without escaping or truncation.
///
/// `#[beta_feature]` calls this once, on the first blocked call of each gated
/// function. It passes the module path and function name joined by `::`, such
/// as `session::attach`.
///
/// `log_blocked("attach")` emits this message:
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
