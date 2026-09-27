//! The `PtyBackend` trait every backend implements, the `PtySink` it delivers
//! each pane's output and exit to, and the `CarriedPtyPane` record a pane is
//! handed on as; see [`crate::backend::state`] for all three.

/// The `PtyBackend` trait, the `PtySink` trait, and the `CarriedPtyPane`
/// record.
pub mod state;
