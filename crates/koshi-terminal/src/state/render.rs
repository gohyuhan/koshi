//! The per-screen rendering state — the pen, the active GL slot, and the
//! `G0`–`G3` charset designations — plus the [`Charset`] each slot can name.

use crate::style::Style;

/// A character set a `G0`–`G3` slot can be designated to, selected into the
/// active GL range by `SI`/`SO` and applied to printed bytes.
///
/// Part of the per-screen [`RenderState`]. Only the three sets real applications
/// use are modeled; an unrecognized designation final byte falls back to
/// [`Ascii`](Charset::Ascii) (a passthrough).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Charset {
    /// US-ASCII (`ESC ( B`): every byte prints as itself. The default.
    #[default]
    Ascii,
    /// DEC Special Character and Line Drawing (`ESC ( 0`): the bytes `0x5F`–
    /// `0x7E` print as box-drawing and symbol glyphs (`q` → `─`, `x` → `│`, …),
    /// so a TUI's `lqqqk` renders `┌───┐`.
    DecLineDrawing,
    /// United Kingdom (`ESC ( A`): identical to ASCII except `#` (`0x23`) prints
    /// as `£`.
    Uk,
}

/// The rendering state that turns a printed byte into a styled glyph: the pen,
/// the active GL slot, and the `G0`–`G3` charset designations.
///
/// Held per screen — the primary and the alternate each own one. Every
/// alternate-screen entry (`?47`/`?1047`/`?1049`) clones the primary's render
/// state into the alternate. DECSC snapshots the active screen's render state
/// into a [`SavedCursor`](crate::state::SavedCursor); DECRC restores it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderState {
    /// The pen applied to printed cells (colors + text attributes).
    pub(in crate::state) style: Style,
    /// The `G0`–`G3` charset designations (`ESC ( ) * +`), indexed by slot.
    pub(in crate::state) charsets: [Charset; 4],
    /// Which `G0`–`G3` slot is invoked into the GL range for printing: `0` after
    /// `SI`, `1` after `SO`.
    pub(in crate::state) gl: usize,
}

impl RenderState {
    /// A fresh render state: default pen, all four slots ASCII, GL on `G0`.
    pub(in crate::state) fn fresh() -> Self {
        RenderState {
            style: Style::default(),
            charsets: [Charset::Ascii; 4],
            gl: 0,
        }
    }
}
