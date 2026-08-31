//! Alternate-screen entry/exit helpers: seed the alternate cursor from the
//! primary, reset the alternate to a fresh buffer, and stash the primary cursor
//! across a `?1049` switch.

use std::sync::Arc;

use crate::state::{SavedCursor, TerminalState};

impl TerminalState {
    /// Copy the primary cursor's position (`row`, `col`) onto the alternate
    /// cursor. Runs on a `?1049` entry, after [`Self::reset_alternate_buffer`].
    /// Visibility, the wrap latch, and the saved stash keep the reset's fresh
    /// values; the screen-switch arm clones the render state on its own.
    pub(super) fn seed_alternate_cursor(&mut self) {
        self.alternate_cursor.row = self.primary_cursor.row;
        self.alternate_cursor.col = self.primary_cursor.col;
    }

    /// Reset the alternate screen to a fresh, blank buffer:
    /// - every cell blanked to the active screen's pen background (BCE,
    ///   background color erase),
    /// - prompt marks cleared,
    /// - scroll region (DECSTBM, the CSI sequence that sets the top/bottom
    ///   scroll margins) back to the full screen,
    /// - cursor home, shown, no wrap latch, no DECSC stash.
    ///
    /// Leaves the alternate's [`RenderState`](crate::state::RenderState) as it
    /// is; the screen-switch arm clones it from the primary on entry, and DECRC
    /// and `RIS` reset it.
    ///
    /// Writes `self.alternate` directly, whichever screen is active. Called by
    /// the `?1049 h` entry and the `?1047 l`/`?1049 l` clearing exits.
    pub(super) fn reset_alternate_buffer(&mut self) {
        let fill = self.active_render().style.bg_fill();
        let alternate = Arc::make_mut(&mut self.alternate);
        let (rows, cols) = alternate.dimensions();
        for row in 0..rows {
            alternate.clear_line(row, 0, cols, fill);
            alternate.set_prompt_mark(row, false);
        }
        self.alternate_scroll_region = None;
        self.alternate_cursor.row = 0;
        self.alternate_cursor.col = 0;
        self.alternate_cursor.is_visible = true;
        self.alternate_cursor.pending_wrap = false;
        self.alternate_cursor.saved = None;
    }

    /// DECSC the primary screen's cursor (`row`, `col`, wrap latch) and render
    /// state into the primary's saved slot, whichever screen is active. Called
    /// by the `?1049` entry.
    pub(super) fn save_primary_cursor(&mut self) {
        self.primary_cursor.saved = Some(SavedCursor {
            row: self.primary_cursor.row,
            col: self.primary_cursor.col,
            pending_wrap: self.primary_cursor.pending_wrap,
            render: self.primary_render,
        });
    }
}
