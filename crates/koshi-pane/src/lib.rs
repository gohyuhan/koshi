//! `koshi-pane` — pane domain: runtime metadata, lifecycle state machine, and
//! policies for every pane in a session.
//!
//! The pane domain owns all per-pane state except content: the [`pane::state::PaneRecord`]
//! (title, command, cwd, lifecycle, exit code), [`pane::policy`] rules (how to close,
//! what happens on process exit), and the [`pane::lifecycle`] state machine. The layout
//! tree holds only [`koshi_core::ids::PaneId`] leaves; the [`registry::PaneRegistry`] is
//! the single owner of everything else, keyed by id. Commands and events (in [`pane::command`]
//! and [`pane::event`]) will route pane operations and notifications through the runtime.

pub mod error;
pub mod types;

pub mod pane;
pub mod registry;
