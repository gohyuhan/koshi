//! Cell styling: foreground and background color plus boolean text attributes,
//! set by SGR (Select Graphic Rendition) escape codes such as `ESC[1m` for
//! bold. `Style` also serves as the "pen": the color/attribute state an app
//! sets that then applies to every character printed until changed again.

/// The visual style of a single cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Style {
    /// Foreground (text) color.
    fg: Color,
    /// Background color.
    bg: Color,
    /// Boolean text attributes (bold, italic, …).
    attrs: AttrFlags,
    /// Underline color (SGR 58). `None` follows the foreground color — the
    /// default state restored by SGR 59.
    underline_color: Option<Color>,
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

    /// Set the underline style (SGR `4` single / `21` double / `24` none).
    pub fn set_underline(&mut self, underline: UnderlineStyle) {
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

    /// Set or clear the faint (decreased-intensity) attribute (SGR `2` / `22`).
    pub fn set_faint(&mut self, faint: bool) {
        self.attrs.faint = faint
    }

    /// Set or clear the blink attribute (SGR `5`/`6` / `25`).
    pub fn set_blink(&mut self, blink: bool) {
        self.attrs.blink = blink
    }

    /// Set or clear the conceal (hidden) attribute (SGR `8` / `28`).
    pub fn set_conceal(&mut self, conceal: bool) {
        self.attrs.conceal = conceal
    }

    /// Set or clear the strikethrough attribute (SGR `9` / `29`).
    pub fn set_strike(&mut self, strike: bool) {
        self.attrs.strike = strike
    }

    /// Set or clear the overline attribute (SGR `53` / `55`).
    pub fn set_overline(&mut self, overline: bool) {
        self.attrs.overline = overline
    }

    /// Set the underline color (SGR `58`), or pass `None` for the default that
    /// follows the foreground color (SGR `59`).
    pub fn set_underline_color(&mut self, underline_color: Option<Color>) {
        self.underline_color = underline_color
    }

    /// The background-color-erase fill style: this pen's background only, with
    /// the foreground and all attributes reset to default. Used to fill cells
    /// cleared by erase, scroll, and resize.
    pub fn bg_fill(&self) -> Self {
        Style {
            fg: Color::Default,
            bg: self.bg,
            attrs: AttrFlags::default(),
            underline_color: None,
        }
    }

    /// The foreground (text) color.
    pub fn fg(&self) -> Color {
        self.fg
    }

    /// The background color.
    pub fn bg(&self) -> Color {
        self.bg
    }

    /// The boolean text attributes (bold, italic, reverse, …).
    pub fn attrs(&self) -> AttrFlags {
        self.attrs
    }

    /// The underline color (SGR 58); `None` follows the foreground color.
    pub fn underline_color(&self) -> Option<Color> {
        self.underline_color
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
    /// Underline style (SGR 4 single / 21 double / 24 none) — one aspect with
    /// mutually exclusive values, so single and double are never both set.
    underline: UnderlineStyle,
    /// Reverse video — swap foreground and background (SGR 7).
    reverse: bool,
    /// Faint / decreased intensity (SGR 2).
    faint: bool,
    /// Blink (SGR 5 slow or 6 rapid, collapsed to one flag).
    blink: bool,
    /// Conceal — hidden text (SGR 8).
    conceal: bool,
    /// Crossed-out / strikethrough (SGR 9).
    strike: bool,
    /// Overline (SGR 53).
    overline: bool,
}

impl AttrFlags {
    /// Bold / increased intensity (SGR 1).
    pub fn bold(&self) -> bool {
        self.bold
    }

    /// Italic (SGR 3).
    pub fn italic(&self) -> bool {
        self.italic
    }

    /// The underline style (SGR 4 / 21 / 24 and the `4:n` forms).
    pub fn underline(&self) -> UnderlineStyle {
        self.underline
    }

    /// Reverse video — swap foreground and background (SGR 7).
    pub fn reverse(&self) -> bool {
        self.reverse
    }

    /// Faint / decreased intensity (SGR 2).
    pub fn faint(&self) -> bool {
        self.faint
    }

    /// Blink (SGR 5 slow or 6 rapid).
    pub fn blink(&self) -> bool {
        self.blink
    }

    /// Conceal — hidden text (SGR 8).
    pub fn conceal(&self) -> bool {
        self.conceal
    }

    /// Crossed-out / strikethrough (SGR 9).
    pub fn strike(&self) -> bool {
        self.strike
    }

    /// Overline (SGR 53).
    pub fn overline(&self) -> bool {
        self.overline
    }
}

/// The underline style of a cell — one rendition aspect with mutually exclusive
/// values, so a cell draws at most one underline and applying a new style
/// replaces the previous one. Selected by SGR 4 / 21 / 24 and the extended
/// `4:n` subparameter forms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UnderlineStyle {
    /// Not underlined (SGR 24 or `4:0`).
    #[default]
    None,
    /// Single underline (SGR 4 or `4:1`).
    Single,
    /// Double underline (SGR 21 or `4:2`).
    Double,
    /// Curly / wavy underline (`4:3`).
    Curly,
    /// Dotted underline (`4:4`).
    Dotted,
    /// Dashed underline (`4:5`).
    Dashed,
}

#[cfg(test)]
mod tests;
