//! `koshi-pane` — the pane domain: runtime metadata, the lifecycle state
//! machine, and the policies for every pane in a session.
//!
//! This crate holds all per-pane state except the pane content.
//! [`pane::state::PaneRecord`] carries the command, the working directory, the
//! lifecycle state and the exit code. [`pane::policy`] sets how a pane closes,
//! and what happens when its process ends. [`pane::lifecycle`] holds the state
//! machine. A layout tree stores only [`koshi_core::ids::PaneId`] leaves.
//! [`registry::PaneRegistry`] owns every record, keyed by id.

pub mod error;
pub mod types;

pub mod pane;
pub mod registry;
