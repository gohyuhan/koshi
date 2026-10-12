//! The columns that text takes on a terminal screen.
//!
//! Each character takes the columns that `unicode-width` gives it, except
//! three characters that take 1 column each: the halfwidth katakana sound
//! marks `U+FF9E` (`ﾞ`) and `U+FF9F` (`ﾟ`), which `unicode-width` gives 0
//! columns, and `U+17D8` (KHMER SIGN BEYYAL), which `unicode-width` gives 3
//! columns. Example: `ｶﾞ` (`U+FF76` `U+FF9E`) takes 2 columns, and `ｶ`
//! followed by the combining voiced sound mark `U+3099` takes 1.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// `U+17D8` KHMER SIGN BEYYAL.
const KHMER_SIGN_BEYYAL: char = '\u{17D8}';

/// Whether `character` is a halfwidth katakana sound mark: `U+FF9E` (`ﾞ`, the
/// voiced sound mark) or `U+FF9F` (`ﾟ`, the semi-voiced sound mark).
#[must_use]
pub fn is_halfwidth_katakana_sound_mark(character: char) -> bool {
    matches!(character, '\u{FF9E}' | '\u{FF9F}')
}

/// The columns `character` takes on its own: 1 for a halfwidth katakana sound
/// mark ([`is_halfwidth_katakana_sound_mark`]) and for `U+17D8`, and the width
/// that `unicode-width` gives every other character. `None` for a control
/// character.
///
/// - `'a'` → `Some(1)`, `'漢'` → `Some(2)`
/// - `'\u{FF9E}'` → `Some(1)`, `'\u{17D8}'` → `Some(1)`
/// - `'\u{301}'`, the combining acute accent → `Some(0)`
/// - `'\u{7}'` → `None`
#[must_use]
pub fn compute_character_display_width(character: char) -> Option<usize> {
    if is_halfwidth_katakana_sound_mark(character) || character == KHMER_SIGN_BEYYAL {
        return Some(1);
    }
    UnicodeWidthChar::width(character)
}

/// The columns `text` takes: the width that `unicode-width` gives `text`, plus
/// 1 for each halfwidth katakana sound mark in it
/// ([`is_halfwidth_katakana_sound_mark`]), minus 2 for each `U+17D8` in it.
///
/// - `"漢字"` → 4, `"e\u{301}"` → 1
/// - `"ｶﾞ"` → 2, `"ｶ\u{3099}"` → 1
/// - `"\u{17D8}x"` → 2
#[must_use]
pub fn compute_text_display_width(text: &str) -> usize {
    let mut display_width = UnicodeWidthStr::width(text);
    for character in text.chars() {
        if is_halfwidth_katakana_sound_mark(character) {
            display_width += 1;
        } else if character == KHMER_SIGN_BEYYAL {
            display_width -= 2;
        }
    }
    display_width
}

#[cfg(test)]
mod tests;
