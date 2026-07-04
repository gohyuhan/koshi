//! `tile-renderer` — ratatui drawing: pane borders, the tabline (tab bar plus
//! the top-right status section), the keybinding hint bar, visible cell
//! rendering, cursor placement, and render snapshots.

pub mod error;
pub mod types;

pub mod render;
pub mod snapshot;

pub use render::{cursor_position, render_frame};
