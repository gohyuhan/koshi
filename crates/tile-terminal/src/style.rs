//! Cell styling: foreground and background color plus boolean text attributes.

/// The visual style of a single cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Style {
    /// Foreground (text) color.
    fg: Color,
    /// Background color.
    bg: Color,
    /// Boolean text attributes (bold, italic, …).
    attrs: AttrFlags,
}

impl Style {
    /// Reset the pen to the terminal default — default colors and no attributes
    /// (SGR `0`).
    pub fn reset(&mut self) {
        *self = Style::default()
    }

    /// Set or clear the bold attribute (SGR `1` / `22`).
    pub fn set_bold(&mut self, bold: bool) {
        self.attrs.bold = bold
    }

    /// Set or clear the italic attribute (SGR `3` / `23`).
    pub fn set_italic(&mut self, italic: bool) {
        self.attrs.italic = italic
    }

    /// Set or clear the underline attribute (SGR `4` / `24`).
    pub fn set_underline(&mut self, underline: bool) {
        self.attrs.underline = underline
    }

    /// Set or clear the reverse-video attribute (SGR `7` / `27`).
    pub fn set_reverse(&mut self, reverse: bool) {
        self.attrs.reverse = reverse
    }

    /// Set the background color (SGR `40`-`47` / `100`-`107` / `48`, or `49`
    /// for the default).
    pub fn set_bg(&mut self, bg_color: Color) {
        self.bg = bg_color
    }

    /// Set the foreground (text) color (SGR `30`-`37` / `90`-`97` / `38`, or
    /// `39` for the default).
    pub fn set_fg(&mut self, fg_color: Color) {
        self.fg = fg_color
    }
}

/// A foreground or background color.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Color {
    /// The terminal's configured default color.
    #[default]
    Default,
    /// A 256-color palette index.
    Indexed(u8),
    /// A 24-bit truecolor value.
    Rgb(u8, u8, u8),
}

/// Boolean SGR text attributes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AttrFlags {
    /// Bold / increased intensity (SGR 1).
    bold: bool,
    /// Italic (SGR 3).
    italic: bool,
    /// Underline (SGR 4).
    underline: bool,
    /// Reverse video — swap foreground and background (SGR 7).
    reverse: bool,
}

#[cfg(test)]
mod tests;
