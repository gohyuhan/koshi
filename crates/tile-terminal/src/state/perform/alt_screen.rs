//! Alternate-screen entry/exit helpers: seed the alternate cursor from the
//! primary, reset the alternate to a fresh buffer, and stash the primary cursor
//! across a `?1049` switch.

use std::sync::Arc;

use crate::state::{SavedCursor, TerminalState};

impl TerminalState {
    /// Copy the primary cursor's *position* onto the alternate cursor on a
    /// `?1049` entry, run after [`Self::reset_alternate_buffer`], so the entering
    /// app starts from where the primary cursor was. The render state is cloned
    /// separately by the screen-switch arm; visibility, the wrap latch, and the
    /// saved stash keep the reset's fresh defaults.
    pub(super) fn seed_alternate_cursor(&mut self) {
        self.alternate_cursor.row = self.primary_cursor.row;
        self.alternate_cursor.col = self.primary_cursor.col;
    }

    /// Reset the alternate screen to a fresh, blank buffer:
    /// - cells blanked to the current pen background (BCE),
    /// - scroll region (DECSTBM) back to the full screen,
    /// - cursor home, shown, no wrap latch, no DECSC stash.
    ///
    /// Leaves the alternate's [`RenderState`](crate::state::RenderState) alone;
    /// the screen-switch arm clones it from the primary on entry, and DECRC and
    /// `RIS` reset it.
    ///
    /// Operates on `self.alternate` directly, not the active grid. Called by the
    /// `?1049 h` entry and the `?1047 l`/`?1049 l` clearing exits.
    pub(super) fn reset_alternate_buffer(&mut self) {
        let fill = self.active_render().style.bg_fill();
        let alternate = Arc::make_mut(&mut self.alternate);
        let (rows, cols) = alternate.dimensions();
        for row in 0..rows {
            alternate.clear_line(row, 0, cols, fill);
        }
        self.alternate_scroll_region = None;
        self.alternate_cursor.row = 0;
        self.alternate_cursor.col = 0;
        self.alternate_cursor.is_visible = true;
        self.alternate_cursor.pending_wrap = false;
        self.alternate_cursor.saved = None;
    }

    /// DECSC the primary screen's cursor and render state into the primary's
    /// saved slot, addressing the primary fields directly. Used by the `?1049`
    /// entry, which must stash the primary regardless of which screen is active
    /// at that point in the mode list.
    pub(super) fn save_primary_cursor(&mut self) {
        self.primary_cursor.saved = Some(SavedCursor {
            row: self.primary_cursor.row,
            col: self.primary_cursor.col,
            pending_wrap: self.primary_cursor.pending_wrap,
            render: self.primary_render,
        });
    }
}
