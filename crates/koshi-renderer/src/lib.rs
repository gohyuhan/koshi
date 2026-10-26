//! `koshi-renderer` — ratatui drawing: pane borders, floating panes and their
//! chrome, the tabline (tab bar plus the right-aligned mode tag), the
//! keybinding statusline, the notification center and the toast, visible cell
//! rendering, cursor placement, the chrome theme, render snapshots, the values
//! each chrome row draws from, the compiled-in region solve that places those
//! rows, and mapping a mouse cell to the region drawn under it.

pub mod floating_pane_chrome;
pub mod floating_pane_tray;
pub mod hit_test;
pub mod images;
pub mod notification;
pub mod placement;
pub mod region;
pub mod render;
pub mod snapshot;
mod statusline_hints;
pub mod theme;

pub use hit_test::{
    compute_clamped_pane_cell, compute_tiled_pane_top_border, find_first_visible_tab_index,
    find_pane_content_rect, resolve_hit_region, HitRegion, TiledPaneTopBorder,
};
pub use images::{
    build_image_cell_snapshot, build_image_paints, draw_image_placeholders, ImageCellSnapshot,
    ImageCellState, ImagePaint, ImagePlacementKey, ImageRenderMode, ImageSourceRect,
    MAX_IMAGE_CELL_SNAPSHOT_CELL_COUNT, TERMINAL_IMAGE_UNAVAILABLE,
};
pub use render::{
    build_frame_geometry, compute_content_rect, compute_pane_area, get_cursor_position,
    get_cursor_style, is_placement_target_pane, list_changed_screen_rows, render_frame, DrawnRows,
    FrameGeometry,
};
