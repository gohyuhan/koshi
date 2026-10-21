//! `koshi-pane` — the pane domain: the runtime metadata of every pane in a
//! session.
//!
//! This crate holds all per-pane state except the pane content.
//! [`pane::state::PaneRecord`] carries the command and the working directory.
//! A layout tree stores only [`koshi_core::ids::PaneId`] leaves.
//! [`registry::PaneRegistry`] owns every record, keyed by id.

pub mod error;

pub mod pane;
pub mod registry;
