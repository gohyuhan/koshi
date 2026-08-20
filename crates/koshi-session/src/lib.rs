//! `koshi-session` — the model of one running session: its tabs and their
//! layout trees, the panes registered in them, and the clients attached to it.
//!
//! Holds the state changes for creating, closing, focusing and reordering tabs
//! and panes, the close/quit cascade a removed pane sets off, focus recovery,
//! and the consistency checks over all of it. Every operation edits session
//! state and returns the events describing what changed; none spawns a process
//! or touches a terminal.

pub mod error;
pub mod types;

pub mod client;
pub mod session;
