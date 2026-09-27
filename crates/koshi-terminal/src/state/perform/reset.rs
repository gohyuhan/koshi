//! Hard and soft terminal reset state changes.

use std::sync::Arc;

use koshi_sixel::SixelPalette;

use crate::grid::state::Grid;
use crate::state::{
    build_default_tab_stops, Cursor, RenderState, Screen, ShellIntegrationState, TerminalModes,
    TerminalState,
};
use crate::style::Style;

impl TerminalState {
    /// DECSTR (`CSI ! p`). On the active screen: shows the cursor, clears the
    /// deferred-wrap latch, drops the saved cursor, resets the pen, charsets, and
    /// GL slot, clears origin mode and the scroll region, and clears both screens'
    /// horizontal margins and DECLRMM. Turns off application cursor keys (`?1`)
    /// and autowrap (`?7`). Ends the in-progress grapheme cluster. Cells,
    /// image placements, the cursor position, tab stops, the title, the
    /// reported cwd, scrollback, both screens' Kitty keyboard flag stacks, and
    /// every other mode stay.
    pub(super) fn apply_soft_reset(&mut self) {
        let cursor = self.get_active_cursor_mut();
        cursor.is_visible = true;
        cursor.is_wrap_pending = false;
        cursor.is_origin_mode_enabled = false;
        cursor.saved = None;

        *self.get_active_render_mut() = RenderState::new();
        *self.scroll_region_mut() = None;
        self.clear_left_right_margin_mode();
        self.modes.is_application_cursor_keys_enabled = false;
        self.modes.is_autowrap_enabled = false;
        self.reset_cluster();
    }

    /// RIS (`ESC c`). Blanks both screens at their current size with the default
    /// style, makes the primary screen active, clears scrollback, homes and
    /// shows both cursors with no wrap latch and no saved cursor, clears both
    /// image-placement lists, resets both render states, every mode, both
    /// vertical and horizontal margin pairs, both Kitty keyboard flag stacks,
    /// the tab stops (every eighth column), the
    /// title, and the OSC 133 shell state, and ends the
    /// in-progress grapheme cluster. The reported cwd, queued device replies,
    /// queued shell-integration facts, and the scrollback's total pushed line
    /// count stay.
    pub(super) fn apply_hard_reset(&mut self) {
        let (row_count, column_count) = self.primary.get_grid_dimensions();
        debug_assert_eq!(
            self.alternate.get_grid_dimensions(),
            (row_count, column_count)
        );

        let grid = Grid::build_blank(row_count, column_count, Style::default());
        self.primary = Arc::new(grid.clone());
        self.alternate = Arc::new(grid);
        self.active_screen = Screen::Primary;
        self.clear_all_image_placements();
        self.scrollback.clear_scrollback();

        let cursor = Cursor {
            row: 0,
            column: 0,
            is_visible: true,
            is_wrap_pending: false,
            is_origin_mode_enabled: false,
            saved: None,
        };
        self.primary_cursor = cursor;
        self.alternate_cursor = cursor;
        self.primary_render = RenderState::new();
        self.alternate_render = RenderState::new();
        self.modes = TerminalModes::default();
        self.sixel_palette = SixelPalette::default();
        self.primary_scroll_region = None;
        self.alternate_scroll_region = None;
        self.primary_horizontal_margins = None;
        self.alternate_horizontal_margins = None;
        self.primary_keyboard_stack.clear_keyboard_flag_entries();
        self.alternate_keyboard_stack.clear_keyboard_flag_entries();
        self.tab_stops = build_default_tab_stops(column_count);
        self.title = None;
        self.shell_integration_state = ShellIntegrationState::default();
        self.reset_cluster();
    }
}
