//! Hard and soft terminal reset state changes.

use std::sync::Arc;

use crate::grid::state::Grid;
use crate::state::{
    default_tab_stops, Cursor, RenderState, Screen, ShellIntegrationState, TerminalModes,
    TerminalState,
};
use crate::style::Style;

impl TerminalState {
    /// DECSTR (`CSI ! p`). On the active screen: shows the cursor, clears the
    /// deferred-wrap latch, drops the saved cursor, resets the pen, charsets, and
    /// GL slot, and clears the scroll region. Turns off application cursor keys
    /// (`?1`) and autowrap (`?7`). Ends the in-progress grapheme cluster. Cells,
    /// the cursor position, tab stops, the title, the reported cwd, scrollback,
    /// and every other mode stay.
    pub(super) fn soft_reset(&mut self) {
        let cursor = self.active_cursor_mut();
        cursor.is_visible = true;
        cursor.pending_wrap = false;
        cursor.saved = None;

        *self.active_render_mut() = RenderState::fresh();
        *self.scroll_region_mut() = None;
        self.modes.app_cursor_keys = false;
        self.modes.autowrap = false;
        self.reset_cluster();
    }

    /// RIS (`ESC c`). Blanks both screens at their current size with the default
    /// style, makes the primary screen active, clears scrollback, homes and
    /// shows both cursors with no wrap latch and no saved cursor, resets both
    /// render states, every mode, both scroll regions, the tab stops (every
    /// eighth column), the title, and the OSC 133 shell state, and ends the
    /// in-progress grapheme cluster. The reported cwd, queued device replies,
    /// queued shell-integration facts, and the scrollback tallies stay.
    pub(super) fn hard_reset(&mut self) {
        let (rows, columns) = self.primary.dimensions();
        debug_assert_eq!(self.alternate.dimensions(), (rows, columns));

        let grid = Grid::blank(rows, columns, Style::default());
        self.primary = Arc::new(grid.clone());
        self.alternate = Arc::new(grid);
        self.active = Screen::Primary;
        self.scrollback.clear();

        let cursor = Cursor {
            row: 0,
            col: 0,
            is_visible: true,
            pending_wrap: false,
            saved: None,
        };
        self.primary_cursor = cursor;
        self.alternate_cursor = cursor;
        self.primary_render = RenderState::fresh();
        self.alternate_render = RenderState::fresh();
        self.modes = TerminalModes::default();
        self.primary_scroll_region = None;
        self.alternate_scroll_region = None;
        self.tab_stops = default_tab_stops(columns);
        self.title = None;
        self.shell_integration_state = ShellIntegrationState::default();
        self.reset_cluster();
    }
}
