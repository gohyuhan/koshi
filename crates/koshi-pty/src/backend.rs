//! The `PtyBackend` trait every backend implements, and the `PtyHandle` a
//! spawned pane is polled through; see [`crate::backend::state`] for both.

/// The `PtyBackend` trait and the `PtyHandle` struct.
pub mod state;

#[cfg(test)]
mod tests;
