//! Bounding and filtering the short strings koshi takes from something it does
//! not control: what a pane's own program reports about itself through
//! `OSC 0/1/2` and `OSC 7`, and what a remote peer reports about its sessions,
//! tabs, panes and server.
//!
//! A pane's screen content is not reported text and passes through untouched.

/// The longest string [`sanitize_reported_text`] returns, in bytes. A longer
/// one is cut at the last character boundary that fits. 512 bytes holds at
/// most 340 display columns.
pub const MAX_REPORTED_TEXT_BYTES: usize = 512;

/// Whether [`sanitize_reported_text`] removes `c`.
///
/// Five classes are removed:
///
/// - control characters ([`char::is_control`], Unicode `Cc`): `U+0000`–`U+001F`,
///   `U+007F`, and `U+0080`–`U+009F`
/// - the twelve Unicode `Bidi_Control` characters: `U+202A`–`U+202E`,
///   `U+2066`–`U+2069`, `U+200E`, `U+200F`, `U+061C`
/// - the line and paragraph separators `U+2028` and `U+2029`
/// - the noncharacters `U+FFFE` and `U+FFFF`, and the interlinear annotation
///   marks `U+FFF9`–`U+FFFB`
/// - the tag characters `U+E0000`–`U+E007F`
///
/// Every other character is kept, including zero-width joiners, combining
/// marks and variation selectors. A tag-sequence flag keeps its base character
/// and loses its region.
fn is_refused(c: char) -> bool {
    matches!(c,
        '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'
        | '\u{200E}' | '\u{200F}' | '\u{061C}'
        | '\u{2028}' | '\u{2029}'
        | '\u{FFF9}'..='\u{FFFB}'
        | '\u{FFFE}' | '\u{FFFF}'
        | '\u{E0000}'..='\u{E007F}'
    ) || c.is_control()
}

/// `raw` with every control, bidi-control, line and paragraph separator,
/// noncharacter, interlinear annotation and tag character removed, cut to
/// [`MAX_REPORTED_TEXT_BYTES`].
///
/// A removed character consumes none of the byte budget. The cut keeps the
/// start and lands on a character boundary, so the result is never longer than
/// [`MAX_REPORTED_TEXT_BYTES`] and never holds a partial character. It may end
/// inside a grapheme cluster: a string cut mid emoji sequence can end on a
/// joiner.
///
/// - `"a\u{7f}b"` → `"ab"`
/// - `"\u{202E}gpj.exe"` → `"gpj.exe"`
/// - 1000 `U+007F` followed by `"shell"` → `"shell"`
/// - 5 MiB of `"a"` → the first 512 of them
/// - `"日"` repeated 1000 times → 510 bytes, 170 characters
#[must_use]
pub fn sanitize_reported_text(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len().min(MAX_REPORTED_TEXT_BYTES));
    for c in raw.chars().filter(|c| !is_refused(*c)) {
        if out.len() + c.len_utf8() > MAX_REPORTED_TEXT_BYTES {
            break;
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests;
