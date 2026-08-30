//! The selection vocabulary the command layer and the event layer share.
//!
//! A [`Selection`] is what
//! [`SetSelectionArgs`](crate::command::SetSelectionArgs) carries and what
//! [`SelectionChanged`](crate::event::SelectionChanged) reports. A
//! [`CopyTarget`] is what [`CopyArgs`](crate::command::CopyArgs) names and what
//! [`Copied`](crate::event::Copied) repeats.
//!
//! These types cross process boundaries, so each one holds only serde-friendly
//! types that mean the same thing in another process.

use serde::{Deserialize, Serialize};

/// The shape of a selection, and with it the gesture that made it: a plain drag
/// selects [`Character`](Self::Character), a double-click drag
/// [`Word`](Self::Word), a triple-click drag [`Line`](Self::Line), and holding
/// `Alt` while dragging [`Block`](Self::Block).
///
/// The kind is fixed when the drag starts and holds for the whole drag:
/// extending a double-click drag keeps snapping to whole words.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SelectionKind {
    /// A contiguous character range that follows the text across soft-wrapped
    /// lines: the end of one row continues at the start of the next.
    Character,
    /// Both ends grown outward to whole words. Dragging from the middle of
    /// `hello` to the middle of `world` selects `hello world` entire.
    Word,
    /// Whole logical lines, soft-wrap included: a line that wrapped over three
    /// rows is selected as all three.
    Line,
    /// A rectangle — the same column range on every row the drag spans, which
    /// is how one column is lifted out of tabular output.
    Block,
}

/// A position in one pane's text, spanning its scrollback history and its live
/// screen as one continuous space.
///
/// A position is a whole cell: the outer terminal reports the pointer as a
/// column and a row and nothing finer. Both ends of a selection are inclusive,
/// so the cell under the pointer is part of the highlight.
///
/// The row is an absolute line number — how many lines the pane had ever pushed
/// into scrollback when this line was the top of the live screen. It counts
/// every line the pane has ever produced and never changes meaning: new output
/// does not renumber it, and neither does the scrollback dropping its oldest
/// lines to stay under its cap. A dropped row is simply gone.
///
/// Example: a pane has pushed 1000 lines into history and its scrollback holds
/// the newest 500 (lines 500..=999). The oldest line you can still scroll back
/// to is row `500`; the top line of the live screen is row `1000`; the row
/// below it is `1001`. Ten more lines of output arrive: the live screen's top
/// line is now row `1010`, and the line that was row `1000` is still row
/// `1000` — now the newest line in history. Cap eviction drops lines 500..=509;
/// the oldest line you can reach is now row `510`, and every surviving line
/// kept the number it had.
///
/// Both numbers come from the running total of lines a pane has pushed into
/// its scrollback and the count it still retains, which the terminal engine
/// tracks as `Scrollback::total_pushed` and `Scrollback::len`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GridPos {
    /// Absolute line number — see the type docs. Never renumbered.
    pub row: u64,
    /// Column in cells, 0-indexed from the left.
    pub col: u16,
}

/// A selection: a highlighted range of text, always made with the mouse — a
/// drag over a pane's content starts one, and a click or any input that reaches
/// the pane's program drops it.
///
/// This one type is both what
/// [`SetSelectionArgs`](crate::command::SetSelectionArgs) carries and what
/// [`SelectionChanged`](crate::event::SelectionChanged) reports.
///
/// Both ends are positions the mouse layer resolved from a drag, and either end
/// may be the earlier one in the text: dragging up or leftward puts `cursor`
/// before `anchor`. Readers that need the range in text order order the pair
/// themselves.
///
/// The pane a selection is in is not a field here — the command
/// ([`SetSelectionArgs::pane`](crate::command::SetSelectionArgs::pane)) and the
/// event
/// ([`SelectionChanged::pane_id`](crate::event::SelectionChanged::pane_id))
/// each name it, and the client keys its highlights by it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Selection {
    /// Selection shape.
    pub kind: SelectionKind,
    /// The end that stays put — where the drag started.
    pub anchor: GridPos,
    /// The end that follows the pointer.
    pub cursor: GridPos,
}

/// Which clipboard a copy targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CopyTarget {
    /// OSC 52 (a terminal escape sequence for setting the clipboard) to the
    /// outer terminal — the default, dependency-free option.
    Osc52,
    /// The native operating-system clipboard. Koshi has no backend for it; a
    /// copy to this target writes nothing.
    Native,
}
