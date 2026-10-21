//! Alternate-screen entry/exit helpers: seed the alternate cursor from the
//! primary, reset the alternate to a fresh buffer, give it a grid on entry,
//! and stash the primary cursor across a `?1049` switch.

use crate::grid::state::Grid;
use crate::state::{AlternateScreen, SavedCursor, TerminalState};

impl TerminalState {
    /// Copy the primary cursor's position (`row`, `column`) onto the alternate
    /// cursor. Runs on a `?1049` entry, after [`Self::reset_alternate_buffer`]
    /// and [`Self::allocate_alternate_grid`]. Visibility, the wrap latch, and
    /// the saved stash keep the reset's fresh values; the screen-switch arm
    /// clones the render state on its own.
    pub(super) fn seed_alternate_cursor(&mut self) {
        self.alternate_cursor.row = self.primary_cursor.row;
        self.alternate_cursor.column = self.primary_cursor.column;
    }

    /// Reset the alternate screen to a fresh, blank buffer:
    /// - the grid freed: the alternate screen becomes
    ///   [`AlternateScreen::Blank`] in the active screen's pen background
    ///   (BCE, background color erase), with no prompt mark,
    /// - scroll region (DECSTBM, the CSI sequence that sets the top/bottom
    ///   scroll margins) back to the full screen,
    /// - horizontal margins (DECSLRM) back to the full width,
    /// - the Kitty keyboard flag stack emptied, which leaves its flags `0`,
    /// - image placements cleared,
    /// - cursor home, shown, no wrap latch, no DECSC stash.
    ///
    /// Leaves the alternate's [`RenderState`](crate::state::RenderState) as it
    /// is; the screen-switch arm clones it from the primary on entry, and DECRC
    /// and `RIS` reset it.
    ///
    /// Writes `self.alternate` directly, whichever screen is active. Called by
    /// the `?1049 h` entry and the `?1047 l`/`?1049 l` clearing exits.
    pub(super) fn reset_alternate_buffer(&mut self) {
        let alternate_fill_style = self.get_active_render().style.get_background_fill_style();
        self.release_alternate_grid(alternate_fill_style);
        self.alternate_scroll_region = None;
        self.alternate_horizontal_margins = None;
        self.alternate_keyboard_stack.clear_keyboard_flag_entries();
        self.alternate_cursor.row = 0;
        self.alternate_cursor.column = 0;
        self.alternate_cursor.is_visible = true;
        self.alternate_cursor.is_wrap_pending = false;
        self.alternate_cursor.is_origin_mode_enabled = false;
        self.alternate_cursor.saved = None;
    }

    /// Give an [`AlternateScreen::Blank`] alternate screen its grid: the
    /// primary screen's size, every cell
    /// [`Cell::build_blank_with_style`](crate::grid::state::Cell::build_blank_with_style)
    /// of the blank's `fill_style`, every row hard-ended with no prompt mark.
    /// An [`AlternateScreen::Allocated`] alternate screen keeps its grid.
    /// Called by every switch to the alternate screen, before the switch.
    pub(in crate::state) fn allocate_alternate_grid(&mut self) {
        if let AlternateScreen::Blank { fill_style } = self.alternate {
            let (row_count, column_count) = self.primary.get_grid_dimensions();
            self.alternate =
                AlternateScreen::Allocated(Grid::build_blank(row_count, column_count, fill_style));
        }
    }

    /// DECSC the primary screen's cursor (`row`, `column`, origin mode, wrap
    /// latch) and render state into the primary's saved slot, whichever screen
    /// is active. Called by the `?1049` entry.
    pub(super) fn save_primary_cursor(&mut self) {
        self.primary_cursor.saved = Some(SavedCursor {
            row: self.primary_cursor.row,
            column: self.primary_cursor.column,
            is_wrap_pending: self.primary_cursor.is_wrap_pending,
            is_origin_mode_enabled: self.primary_cursor.is_origin_mode_enabled,
            render: self.primary_render,
        });
    }
}
