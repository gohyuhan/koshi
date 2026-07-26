//! Pane domain organization: metadata, policies, lifecycle state machine, and
//! event/command routing.
//!
//! - [`state`]: per-pane runtime metadata (`PaneRecord`) and pane kind (terminal or plugin).
//! - [`policy`]: how panes close (graceful, forced, confirm) and what happens on exit.
//! - [`lifecycle`]: state machine for pane spawn, running, exit, close, and removal.
//! - [`command`]: placeholder module, no items yet.
//! - [`event`]: placeholder module, no items yet.

pub mod command;
pub mod event;
pub mod lifecycle;
pub mod policy;
pub mod state;

#[cfg(test)]
mod tests;
