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
