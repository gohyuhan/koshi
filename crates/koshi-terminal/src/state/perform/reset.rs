//! Hard and soft terminal reset state changes.

use std::sync::Arc;

use crate::grid::state::Grid;
use crate::state::{Cursor, RenderState, Screen, TerminalModes, TerminalState};
use crate::style::Style;

use super::super::default_tab_stops;

impl TerminalState {
    /// Reset the active screen's DEC state while keeping its cells and cursor position.
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

    /// Restore terminal display state to its initial values.
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
        self.reset_cluster();
    }
}
