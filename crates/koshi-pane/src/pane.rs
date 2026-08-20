//! Pane domain modules: metadata, policies, and the lifecycle state machine.
//!
//! - [`state`]: the per-pane runtime record, and the pane kind.
//! - [`policy`]: how a pane closes, and what happens when its process ends.
//! - [`lifecycle`]: the state machine from spawn to removal.

pub mod lifecycle;
pub mod policy;
pub mod state;

#[cfg(test)]
mod tests;
