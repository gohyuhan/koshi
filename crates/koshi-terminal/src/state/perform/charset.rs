//! Charset translation: designate the `G0`–`G3` slots and map printed bytes
//! through the active GL charset (DEC line drawing, UK), so a TUI's (text user
//! interface — a terminal app, like an editor or file manager, that draws with
//! characters) `lqqqk` renders `┌───┐`.

use crate::state::{Charset, TerminalState};

impl TerminalState {
    /// The charset currently selected into GL — the active screen's render
    /// state's `G0`–`G3` slot named by its `gl` — used to translate each printed
    /// byte.
    fn active_charset(&self) -> Charset {
        let render = self.active_render();
        render.charsets[render.gl]
    }

    /// Translate a printable `c` through the active GL charset before it is
    /// placed. ASCII passes everything through; DEC line-drawing remaps the
    /// `0x5F`–`0x7E` range to box-drawing/symbol glyphs; UK remaps only `#`.
    /// Every output glyph is a single narrow, non-combining `char`, so the rest
    /// of `print` (cluster folding, width) is unaffected.
    pub(super) fn map_charset(&self, c: char) -> char {
        match self.active_charset() {
            Charset::Ascii => c,
            Charset::DecLineDrawing => map_dec_line_drawing(c),
            Charset::Uk if c == '#' => '£',
            Charset::Uk => c,
        }
    }

    /// Designate the `G0`–`G3` slot `index` (from the `ESC ( ) * +`
    /// intermediate) to the charset named by the final `byte`: `0` = DEC line
    /// drawing, `B` = ASCII, `A` = UK; any other final falls back to ASCII (a
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

/// Map a `char` through the DEC Special Character and Line Drawing set (`ESC (
/// 0`): the bytes `0x5F`–`0x7E` (`'_'`–`'~'`) become box-drawing and symbol
/// glyphs, so a TUI's `lqqqk` renders `┌───┐`. Every byte outside that range,
/// and any unmapped byte within it, passes through unchanged. The table is the
/// VT100 set as implemented by xterm/alacritty (`StandardCharset::map`); all
/// outputs are single narrow, non-combining glyphs.
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
