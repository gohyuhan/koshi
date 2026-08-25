//! `koshi-renderer` — ratatui drawing: pane borders, the tabline (tab bar plus
//! the right-aligned mode tag), the keybinding hint bar, visible cell
//! rendering, cursor placement, the chrome theme, render snapshots, the values
//! each chrome row draws from, and mapping a mouse cell to the region drawn
//! under it.

pub mod hit_test;
pub mod region;
pub mod render;
pub mod snapshot;
pub mod statusline_hints;
pub mod theme;

pub use hit_test::{
    hit_test, pane_cell_clamped, pane_content_rect, pane_local_cell, tabline_first_visible,
    HitRegion,
};
pub use render::{cursor_position, cursor_style, render_frame};
