//! A screen row trimmed to the renderer's inner width, guarding the right edge
//! against a half-drawn wide glyph.

use crate::grid::state::Cell;

/// One screen row trimmed to the renderer's inner width, with a flag for a
/// wide glyph clipped at the right edge.
///
/// Produced by [`TerminalState::clip_row`](crate::state::TerminalState::clip_row).
/// Borrows the live grid row, so it lives only as long as that borrow. A wide
/// glyph (CJK, emoji) occupies two columns; when the inner rect ends between its
/// halves, drawing only the left half would show a broken glyph. `clip_row`
/// instead drops that base from `cells` and sets `right_pad`, telling the
/// renderer to fill the freed column with a blank.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClippedRow<'a> {
    /// The visible cells, left to right. When `right_pad` is set this stops one
    /// column short of the inner width — the clipped wide base is excluded.
    pub(in crate::state) cells: &'a [Cell],
    /// `true` when the last visible column would have shown only the left half
    /// of a wide glyph; the renderer draws one blank pad cell there instead.
    pub(in crate::state) right_pad: bool,
}

impl<'a> ClippedRow<'a> {
    /// The visible cells, left to right. The renderer draws these, then one
    /// blank pad cell when `right_pad` is set. The slice borrows the underlying
    /// grid row, so it outlives this `ClippedRow`.
    pub fn cells(&self) -> &'a [Cell] {
        self.cells
    }

    /// Whether the renderer should draw one blank pad cell after `cells` to
    /// fill the column a clipped wide glyph would have half-occupied.
    pub fn right_pad(&self) -> bool {
        self.right_pad
    }
}
