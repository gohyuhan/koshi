//! The text cursor and the cursor/render snapshot saved by DECSC/DECRC.

use super::render::RenderState;

/// A cursor position and the render state captured by DECSC, restored by DECRC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SavedCursor {
    /// Saved zero-based row within the grid.
    pub(in crate::state) row: u16,
    /// Saved zero-based column within the grid.
    pub(in crate::state) col: u16,
    /// The deferred-wrap latch at save time, restored alongside the position so
    /// a glyph parked at the last column still wraps after a save/restore.
    pub(in crate::state) pending_wrap: bool,
    /// Snapshot of the active screen's [`RenderState`] (pen, charsets, GL slot)
    /// at save time. DECSC/DECRC carry the whole render state with the cursor, so
    /// an app that changes the pen or a designation, saves, changes it again,
    /// then restores gets the original back.
    pub(in crate::state) render: RenderState,
}

/// The text cursor: position, visibility, and the deferred-wrap latch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    /// Zero-based row within the active grid (internally 0-based despite
    /// 1-based ANSI addressing).
    pub(in crate::state) row: u16,
    /// Zero-based column within the active grid.
    pub(in crate::state) col: u16,
    /// Whether the cursor is currently shown (toggled by DEC mode `?25`).
    pub(in crate::state) is_visible: bool,
    /// Deferred-wrap latch (xterm-style): set when a glyph is printed into the
    /// last column, leaving the cursor parked there instead of advancing. The
    /// next printable glyph first wraps to the following line, so a row that
    /// exactly fills the width does not scroll early. Any cursor-moving
    /// operation clears it.
    pub(in crate::state) pending_wrap: bool,
    /// Saved cursor position and style from DECSC/DECRC (xterm form) or
    /// SCOSC/SCORC (ANSI form), kept per screen so each screen buffer has its
    /// own snapshot independent of the other.
    pub(in crate::state) saved: Option<SavedCursor>,
}

#[cfg(test)]
mod tests;
