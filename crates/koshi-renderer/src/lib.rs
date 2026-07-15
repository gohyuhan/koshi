//! `koshi-renderer` — ratatui drawing: pane borders, the tabline (tab bar plus
//! the top-right status section), the keybinding hint bar, visible cell
//! rendering, cursor placement, and render snapshots.

pub mod error;
pub mod types;

pub mod hit_test;
pub mod render;
pub mod snapshot;
pub mod statusline_hints;
pub mod theme;

pub use hit_test::{
    hit_test, pane_content_rect, pane_local_cell, tabline_first_visible, HitRegion,
};
pub use render::{cursor_position, cursor_style, render_frame};
