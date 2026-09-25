//! `koshi-renderer` — ratatui drawing: pane borders, the tabline (tab bar plus
//! the right-aligned mode tag), the keybinding statusline, visible cell
//! rendering, cursor placement, the chrome theme, render snapshots, the values
//! each chrome row draws from, the compiled-in region solve that places those
//! rows, and mapping a mouse cell to the region drawn under it.

pub mod hit_test;
pub mod images;
pub mod region;
pub mod render;
pub mod snapshot;
mod statusline_hints;
pub mod theme;

pub use hit_test::{
    compute_clamped_pane_cell, compute_pane_local_cell, compute_placement_handle_rect,
    find_first_visible_tab_index, hit_test, is_placement_handle_cell, pane_content_rect, HitRegion,
    PLACEMENT_HANDLE_COLUMN_COUNT,
};
pub use images::{
    build_image_cell_snapshot, build_image_paints, draw_image_placeholders, ImageCellSnapshot,
    ImageCellState, ImagePaint, ImagePlacementKey, ImageRenderMode, ImageSourceRect,
    MAX_IMAGE_CELL_SNAPSHOT_CELL_COUNT, TERMINAL_IMAGE_UNAVAILABLE,
};
pub use render::{
    compute_content_rect, compute_pane_area, get_cursor_position, get_cursor_style,
    is_placement_target_pane, render_frame, render_frame_with_images,
    render_frame_with_placement_target,
};
