//! Cell foreground/background colors and boolean text attributes set by SGR
//! (Select Graphic Rendition) escape codes such as `ESC[1m` for bold. `Style`
//! stores the pen state applied to each printed character until it changes.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize};

/// The visual style of a single cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Style {
    /// Foreground (text) color.
    foreground_color: Color,
    /// Background color.
    background_color: Color,
    /// Boolean text attributes (bold, italic, …).
    attributes: AttrFlags,
    /// Underline color (SGR `58`). `None`, the default restored by SGR `59`,
    /// follows the foreground color.
    underline_color: Option<Color>,
}

impl Style {
    /// Reset the pen to terminal defaults: default colors, no attributes, and
    /// no underline color (SGR `0`).
    pub fn reset_style(&mut self) {
        *self = Style::default();
    }

    /// Set or clear the bold attribute (SGR `1` / `22`).
    pub fn set_bold(&mut self, is_bold: bool) {
        self.attributes.set_attribute_bit(AttrFlags::BOLD, is_bold);
    }

    /// Set or clear the italic attribute (SGR `3` / `23`).
    pub fn set_italic(&mut self, is_italic: bool) {
        self.attributes
            .set_attribute_bit(AttrFlags::ITALIC, is_italic);
    }

    /// Set the underline style (SGR `4` single / `21` double / `24` none).
    pub fn set_underline(&mut self, underline: UnderlineStyle) {
        self.attributes.set_underline(underline);
    }

    /// Set or clear the reverse-video attribute (SGR `7` / `27`).
    pub fn set_reverse(&mut self, is_reverse: bool) {
        self.attributes
            .set_attribute_bit(AttrFlags::REVERSE, is_reverse);
    }

    /// Set the background color (SGR `40`-`47` / `100`-`107` / `48`, or `49`
    /// for the default).
    pub fn set_background_color(&mut self, background_color: Color) {
        self.background_color = background_color;
    }

    /// Set the foreground (text) color (SGR `30`-`37` / `90`-`97` / `38`, or
    /// `39` for the default).
    pub fn set_foreground_color(&mut self, foreground_color: Color) {
        self.foreground_color = foreground_color;
    }

    /// Set or clear the faint (decreased-intensity) attribute (SGR `2` / `22`).
    pub fn set_faint(&mut self, is_faint: bool) {
        self.attributes
            .set_attribute_bit(AttrFlags::FAINT, is_faint);
    }

    /// Set or clear the blink attribute (SGR `5`/`6` / `25`).
    pub fn set_blink(&mut self, is_blinking: bool) {
        self.attributes
            .set_attribute_bit(AttrFlags::BLINK, is_blinking);
    }

    /// Set or clear the conceal (hidden) attribute (SGR `8` / `28`).
    pub fn set_conceal(&mut self, is_concealed: bool) {
        self.attributes
            .set_attribute_bit(AttrFlags::CONCEAL, is_concealed);
    }

    /// Set or clear the strikethrough attribute (SGR `9` / `29`).
    pub fn set_strike(&mut self, is_strikethrough: bool) {
        self.attributes
            .set_attribute_bit(AttrFlags::STRIKE, is_strikethrough);
    }

    /// Set or clear the overline attribute (SGR `53` / `55`).
    pub fn set_overline(&mut self, is_overlined: bool) {
        self.attributes
            .set_attribute_bit(AttrFlags::OVERLINE, is_overlined);
    }

    /// Set the underline color (SGR `58`), or pass `None` for the default that
    /// follows the foreground color (SGR `59`).
    pub fn set_underline_color(&mut self, underline_color: Option<Color>) {
        self.underline_color = underline_color;
    }

    /// Return a background erase style with this background and default
    /// foreground color, attributes, and underline color.
    pub fn get_background_fill_style(&self) -> Self {
        Style {
            background_color: self.background_color,
            ..Style::default()
        }
    }

    /// The foreground (text) color.
    pub fn get_foreground_color(&self) -> Color {
        self.foreground_color
    }

    /// The background color.
    pub fn get_background_color(&self) -> Color {
        self.background_color
    }

    /// The boolean text attributes (bold, italic, reverse, …).
    pub fn get_attributes(&self) -> AttrFlags {
        self.attributes
    }

    /// The underline color (SGR 58); `None` follows the foreground color.
    pub fn get_underline_color(&self) -> Option<Color> {
        self.underline_color
    }
}

/// A foreground or background color.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Color {
    /// The terminal's configured default color.
    #[default]
    Default,
    /// A 256-color palette index.
    Indexed(u8),
    /// A 24-bit truecolor value.
    Rgb(u8, u8, u8),
}

/// SGR text attributes packed into one 16-bit word: eight boolean attributes
/// in bits 0-7, the underline style as a 3-bit code in bits 8-10, and five
/// spare bits. `ESC[1;3m` (bold and italic) gives `0b11`; `ESC[4;9m` (single
/// underline and strikethrough) gives `1 << 6 | 1 << 8` = 320. A new boolean
/// attribute takes one of the spare bits. Serializes as the bare `u16`.
///
/// Reading one back keeps only what the getters read: the five spare bits are
/// dropped, and an underline code of `6` or `7` — which names no style —
/// becomes `0`. Two words that every getter reads the same way are equal.
#[derive(Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AttrFlags(#[serde(deserialize_with = "deserialize_defined_attribute_bits")] u16);

/// Deserialize an attribute word, keeping only the bits the getters read.
///
/// `1 << 15` becomes `0`; `0b110 << 8` (underline code `6`) becomes `0`;
/// `0b001 << 8 | 1` (single underline and bold) is kept whole.
fn deserialize_defined_attribute_bits<'de, DeserializerType: Deserializer<'de>>(
    deserializer: DeserializerType,
) -> Result<u16, DeserializerType::Error> {
    let attribute_word = u16::deserialize(deserializer)? & AttrFlags::DEFINED_MASK;
    let underline_code = (attribute_word & AttrFlags::UNDERLINE_MASK) >> AttrFlags::UNDERLINE_SHIFT;
    if UnderlineStyle::from_underline_code(underline_code) == UnderlineStyle::None {
        return Ok(attribute_word & !AttrFlags::UNDERLINE_MASK);
    }
    Ok(attribute_word)
}

impl AttrFlags {
    /// Bold / increased intensity (SGR 1).
    const BOLD: u16 = 1 << 0;
    /// Italic (SGR 3).
    const ITALIC: u16 = 1 << 1;
    /// Reverse video (SGR 7).
    const REVERSE: u16 = 1 << 2;
    /// Faint / decreased intensity (SGR 2).
    const FAINT: u16 = 1 << 3;
    /// Blink (SGR 5 slow or 6 rapid, collapsed to one flag).
    const BLINK: u16 = 1 << 4;
    /// Conceal — hidden text (SGR 8).
    const CONCEAL: u16 = 1 << 5;
    /// Crossed-out / strikethrough (SGR 9).
    const STRIKE: u16 = 1 << 6;
    /// Overline (SGR 53).
    const OVERLINE: u16 = 1 << 7;
    /// Where the underline code starts: bits 8-10 hold it.
    const UNDERLINE_SHIFT: u16 = 8;
    /// The three bits the underline code occupies, in place.
    const UNDERLINE_MASK: u16 = 0b111 << Self::UNDERLINE_SHIFT;
    /// The eleven bits the getters read: the eight boolean attributes and the
    /// underline code. The five above them are spare.
    const DEFINED_MASK: u16 = 0x07FF;

    /// Whether the single-bit `bit` is set.
    fn has_attribute_bit(self, attribute_bit: u16) -> bool {
        self.0 & attribute_bit != 0
    }

    /// Set `attribute_bit` when `is_enabled` is true; clear it when it is false.
    fn set_attribute_bit(&mut self, attribute_bit: u16, is_enabled: bool) {
        if is_enabled {
            self.0 |= attribute_bit;
        } else {
            self.0 &= !attribute_bit;
        }
    }

    /// Replace the underline code with `underline`'s, leaving every other bit
    /// as it was.
    fn set_underline(&mut self, underline: UnderlineStyle) {
        self.0 = (self.0 & !Self::UNDERLINE_MASK)
            | (underline.get_underline_code() << Self::UNDERLINE_SHIFT);
    }

    /// Bold / increased intensity (SGR 1).
    pub fn is_bold(&self) -> bool {
        self.has_attribute_bit(Self::BOLD)
    }

    /// Italic (SGR 3).
    pub fn is_italic(&self) -> bool {
        self.has_attribute_bit(Self::ITALIC)
    }

    /// The underline style (SGR 4 / 21 / 24 and the `4:n` forms).
    pub fn get_underline_style(&self) -> UnderlineStyle {
        UnderlineStyle::from_underline_code(
            (self.0 & Self::UNDERLINE_MASK) >> Self::UNDERLINE_SHIFT,
        )
    }

    /// Reverse video — swap foreground and background (SGR 7).
    pub fn is_reverse(&self) -> bool {
        self.has_attribute_bit(Self::REVERSE)
    }

    /// Faint / decreased intensity (SGR 2).
    pub fn is_faint(&self) -> bool {
        self.has_attribute_bit(Self::FAINT)
    }

    /// Blink (SGR 5 slow or 6 rapid).
    pub fn is_blinking(&self) -> bool {
        self.has_attribute_bit(Self::BLINK)
    }

    /// Conceal — hidden text (SGR 8).
    pub fn is_concealed(&self) -> bool {
        self.has_attribute_bit(Self::CONCEAL)
    }

    /// Crossed-out / strikethrough (SGR 9).
    pub fn is_strikethrough(&self) -> bool {
        self.has_attribute_bit(Self::STRIKE)
    }

    /// Overline (SGR 53).
    pub fn is_overlined(&self) -> bool {
        self.has_attribute_bit(Self::OVERLINE)
    }
}

impl fmt::Debug for AttrFlags {
    /// Lists the attributes that are on, booleans first in declaration order,
    /// then the underline style. Default flags print `AttrFlags(none)`; bold
    /// with a single underline prints `AttrFlags(bold, underline)`; a curly
    /// underline alone prints `AttrFlags(curly-underline)`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut active_attribute_names: Vec<&str> = [
            ("bold", Self::BOLD),
            ("italic", Self::ITALIC),
            ("reverse", Self::REVERSE),
            ("faint", Self::FAINT),
            ("blink", Self::BLINK),
            ("conceal", Self::CONCEAL),
            ("strike", Self::STRIKE),
            ("overline", Self::OVERLINE),
        ]
        .into_iter()
        .filter(|&(_, attribute_bit)| self.has_attribute_bit(attribute_bit))
        .map(|(attribute_name, _)| attribute_name)
        .collect();
        match self.get_underline_style() {
            UnderlineStyle::None => {}
            UnderlineStyle::Single => active_attribute_names.push("underline"),
            UnderlineStyle::Double => active_attribute_names.push("double-underline"),
            UnderlineStyle::Curly => active_attribute_names.push("curly-underline"),
            UnderlineStyle::Dotted => active_attribute_names.push("dotted-underline"),
            UnderlineStyle::Dashed => active_attribute_names.push("dashed-underline"),
        }
        if active_attribute_names.is_empty() {
            active_attribute_names.push("none");
        }
        write!(f, "AttrFlags({})", active_attribute_names.join(", "))
    }
}

/// The underline style of a cell. A cell has exactly one; setting a new one
/// replaces the previous one. Selected by SGR 4 / 21 / 24 and the extended
/// `4:n` subparameter forms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
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

impl UnderlineStyle {
    /// This style's 3-bit code, as stored in [`AttrFlags`]. The codes are the
    /// `4:n` subparameter numbers: `None` is 0, `Curly` is 3.
    fn get_underline_code(self) -> u16 {
        match self {
            UnderlineStyle::None => 0,
            UnderlineStyle::Single => 1,
            UnderlineStyle::Double => 2,
            UnderlineStyle::Curly => 3,
            UnderlineStyle::Dotted => 4,
            UnderlineStyle::Dashed => 5,
        }
    }

    /// The style a 3-bit `code` names: `1`-`5` give `Single` through `Dashed`;
    /// `0`, `6`, and `7` give `None`.
    fn from_underline_code(underline_code: u16) -> Self {
        match underline_code {
            1 => UnderlineStyle::Single,
            2 => UnderlineStyle::Double,
            3 => UnderlineStyle::Curly,
            4 => UnderlineStyle::Dotted,
            5 => UnderlineStyle::Dashed,
            _ => UnderlineStyle::None,
        }
    }
}

#[cfg(test)]
mod tests;
