//! Charset translation: designate the `G0`–`G3` slots and map printed bytes
//! through the active GL charset (DEC line drawing, UK), so a TUI's (text user
//! interface — a terminal app, like an editor or file manager, that draws with
//! characters) `lqqqk` renders `┌───┐`.

use crate::state::{Charset, TerminalState};

impl TerminalState {
    /// The charset selected into GL: the active screen's `G0`–`G3` slot named
    /// by its `gl`. Every printed byte is translated through it.
    fn active_charset(&self) -> Charset {
        let render = self.active_render();
        render.charsets[render.gl]
    }

    /// Translate a printable `c` through the active GL charset. ASCII passes
    /// every char through; DEC line drawing remaps `0x5F`–`0x7E` to box-drawing
    /// and symbol glyphs; UK remaps only `#` (to `£`). Every output glyph is
    /// one narrow, non-combining `char`.
    pub(super) fn map_charset(&self, c: char) -> char {
        match self.active_charset() {
            Charset::Ascii => c,
            Charset::DecLineDrawing => map_dec_line_drawing(c),
            Charset::Uk if c == '#' => '£',
            Charset::Uk => c,
        }
    }

    /// Designate the `G0`–`G3` slot `index` (`0`–`3`, from the `ESC ( ) * +`
    /// intermediate) to the charset named by the final `byte`: `0` = DEC line
    /// drawing, `B` = ASCII, `A` = UK; any other final selects ASCII (a
    /// passthrough). Writes the active screen's render state.
    pub(super) fn designate_charset(&mut self, index: usize, byte: u8) {
        let charset = match byte {
            b'0' => Charset::DecLineDrawing,
            b'B' => Charset::Ascii,
            b'A' => Charset::Uk,
            _ => Charset::Ascii,
        };
        self.active_render_mut().charsets[index] = charset;
    }
}

/// Map `c` through the DEC Special Character and Line Drawing set (`ESC ( 0`):
/// each of the 32 bytes `0x5F`–`0x7E` (`'_'`–`'~'`) becomes a box-drawing or
/// symbol glyph, so a TUI's `lqqqk` renders `┌───┐`. Every char outside that
/// range passes through unchanged. Every output is one narrow, non-combining
/// glyph.
fn map_dec_line_drawing(c: char) -> char {
    match c {
        '_' => ' ',
        '`' => '◆',
        'a' => '▒',
        'b' => '\u{2409}', // ␉ symbol for horizontal tab
        'c' => '\u{240c}', // ␌ symbol for form feed
        'd' => '\u{240d}', // ␍ symbol for carriage return
        'e' => '\u{240a}', // ␊ symbol for line feed
        'f' => '°',
        'g' => '±',
        'h' => '\u{2424}', // ␤ symbol for newline
        'i' => '\u{240b}', // ␋ symbol for vertical tab
        'j' => '┘',
        'k' => '┐',
        'l' => '┌',
        'm' => '└',
        'n' => '┼',
        'o' => '⎺', // scan line 1
        'p' => '⎻', // scan line 3
        'q' => '─', // scan line 5 (horizontal)
        'r' => '⎼', // scan line 7
        's' => '⎽', // scan line 9
        't' => '├',
        'u' => '┤',
        'v' => '┴',
        'w' => '┬',
        'x' => '│', // vertical
        'y' => '≤',
        'z' => '≥',
        '{' => 'π',
        '|' => '≠',
        '}' => '£',
        '~' => '·',
        _ => c,
    }
}
