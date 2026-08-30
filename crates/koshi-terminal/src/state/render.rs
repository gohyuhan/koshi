//! The per-screen rendering state — the pen, the active GL slot, and the
//! `G0`–`G3` charset designations — plus the [`Charset`] each slot can name.

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize};

use crate::style::Style;

/// A character set a `G0`–`G3` slot can be designated to, selected into the
/// active GL range by `SI`/`SO` and applied to printed bytes.
///
/// Part of the per-screen [`RenderState`]. An unrecognized designation final
/// byte selects [`Ascii`](Charset::Ascii).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Charset {
    /// US-ASCII (`ESC ( B`): every byte prints as itself. The default.
    #[default]
    Ascii,
    /// DEC Special Character and Line Drawing (`ESC ( 0`): the bytes `0x5F`–
    /// `0x7E` print as box-drawing and symbol glyphs (`q` → `─`, `x` → `│`, …);
    /// `lqqqk` renders `┌───┐`.
    DecLineDrawing,
    /// United Kingdom (`ESC ( A`): identical to ASCII except `#` (`0x23`) prints
    /// as `£`.
    Uk,
}

/// The rendering state that turns a printed byte into a styled glyph: the pen,
/// the active GL slot, and the `G0`–`G3` charset designations.
///
/// Held per screen — the primary and the alternate each own one. A switch
/// from the primary to the alternate screen (`?47`/`?1047`/`?1049`) copies the
/// primary's render state into the alternate. DECSC snapshots the active
/// screen's render state into a [`SavedCursor`](crate::state::SavedCursor);
/// DECRC restores it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenderState {
    /// The pen applied to printed cells (colors + text attributes).
    pub(in crate::state) style: Style,
    /// The `G0`–`G3` charset designations (`ESC ( ) * +`), indexed by slot.
    pub(in crate::state) charsets: [Charset; 4],
    /// Which `G0`–`G3` slot is invoked into the GL range for printing: `0` after
    /// `SI`, `1` after `SO`. Indexes `charsets`, so it stays below `4`.
    #[serde(deserialize_with = "gl_slot")]
    pub(in crate::state) gl: usize,
}

/// Read a GL slot, refusing one that does not index the four charset slots.
///
/// The wire form is a bare number. A `4` gives the error `GL slot must be 0-3`,
/// which reaches a resume file's reader as a corrupt body.
fn gl_slot<'de, D: Deserializer<'de>>(deserializer: D) -> Result<usize, D::Error> {
    let gl = usize::deserialize(deserializer)?;
    if gl > 3 {
        return Err(D::Error::custom("GL slot must be 0-3"));
    }
    Ok(gl)
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

#[cfg(test)]
mod tests;
