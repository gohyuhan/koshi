//! The `PtyBackend` trait every backend implements, the `PtyHandle` a spawned
//! pane is polled through, and the `CarriedPtyPane` record a pane is handed on
//! as; see [`crate::backend::state`] for all three.

/// The `PtyBackend` trait, the `PtyHandle` struct, and the `CarriedPtyPane`
/// record.
pub mod state;
