//! SGR (Select Graphic Rendition) handling: apply a `CSI … m` sequence to the
//! pen [`Style`], including the 256-color and truecolor extended selectors.

use crate::style::{Color, Style, UnderlineStyle};

/// Apply an SGR (Select Graphic Rendition, `CSI … m`) sequence to `style`:
/// update the pen colors and text attributes carried by subsequently printed
/// cells. Empty parameters are an implicit reset (equivalent to SGR `0`); the
/// extended-color selectors `38`/`48` are parsed by [`extended_color`].
pub(super) fn apply_sgr(style: &mut Style, params: &vte::Params) {
    if params.is_empty() {
        style.reset();
        return;
    }

    let mut iter = params.iter();
    while let Some(p) = iter.next() {
        // Dispatch on the SGR code number `p.first()`; an empty parameter (e.g.
        // `CSI ;m`) carries no value, so `unwrap_or(0)` makes it code 0 (reset).
        // Each arm's comment names the code so the mapping reads without the spec.
        match p.first().copied().unwrap_or(0) {
            0 => style.reset(),          // 0: reset all attributes + colors
            1 => style.set_bold(true),   // 1: bold (increased intensity)
            2 => style.set_faint(true),  // 2: faint (decreased intensity)
            3 => style.set_italic(true), // 3: italic
            // 4: underline. An optional `4:n` subparameter selects the style and
            // is grouped into this param slice; bare `4` and `4:1` are single,
            // `4:0` cancels, `4:2`-`4:5` are double/curly/dotted/dashed, any
            // other subparameter is single.
            4 => {
                let underline = match p.get(1).copied() {
                    Some(0) => UnderlineStyle::None,
                    Some(2) => UnderlineStyle::Double,
                    Some(3) => UnderlineStyle::Curly,
                    Some(4) => UnderlineStyle::Dotted,
                    Some(5) => UnderlineStyle::Dashed,
                    _ => UnderlineStyle::Single,
                };
                style.set_underline(underline);
            }
            5 | 6 => style.set_blink(true), // 5/6: blink (slow/rapid → one flag)
            7 => style.set_reverse(true),   // 7: reverse video (swap fg/bg)
            8 => style.set_conceal(true),   // 8: conceal (hidden)
            9 => style.set_strike(true),    // 9: crossed-out (strikethrough)
            21 => style.set_underline(UnderlineStyle::Double), // 21: double underline
            // 22: normal intensity — cancels both bold (1) and faint (2).
            22 => {
                style.set_bold(false);
                style.set_faint(false);
            }
            23 => style.set_italic(false), // 23: italic off
            24 => style.set_underline(UnderlineStyle::None), // 24: not underlined (cancels 4 and 21)
            25 => style.set_blink(false),                    // 25: blink off
            27 => style.set_reverse(false),                  // 27: reverse off
            28 => style.set_conceal(false),                  // 28: reveal (conceal off)
            29 => style.set_strike(false),                   // 29: strikethrough off
            c @ 30..=37 => style.set_fg(Color::Indexed((c - 30) as u8)), // 30-37: fg palette 0-7
            c @ 90..=97 => style.set_fg(Color::Indexed((c - 90 + 8) as u8)), // 90-97: bright fg 8-15
            39 => style.set_fg(Color::Default),                              // 39: default fg
            c @ 40..=47 => style.set_bg(Color::Indexed((c - 40) as u8)), // 40-47: bg palette 0-7
            c @ 100..=107 => style.set_bg(Color::Indexed((c - 100 + 8) as u8)), // 100-107: bright bg 8-15
            49 => style.set_bg(Color::Default),                                 // 49: default bg
            53 => style.set_overline(true),                                     // 53: overline
            55 => style.set_overline(false),                                    // 55: overline off
            // 38: extended fg — 256-palette (`38;5;n`) or truecolor (`38;2;r;g;b`).
            38 => {
                if let Some(col) = extended_color(p, &mut iter) {
                    style.set_fg(col);
                }
            }
            // 48: extended bg — 256-palette (`48;5;n`) or truecolor (`48;2;r;g;b`).
            48 => {
                if let Some(col) = extended_color(p, &mut iter) {
                    style.set_bg(col);
                }
            }
            // 58: underline color — same 256-palette / truecolor forms as 38/48.
            58 => {
                if let Some(col) = extended_color(p, &mut iter) {
                    style.set_underline_color(Some(col));
                }
            }
            59 => style.set_underline_color(None), // 59: default underline color
            _ => {}                                // unknown / out-of-scope SGR code: ignore
        }
    }
}

/// The primary value of the iterator's next CSI parameter, or `None` when the
/// iterator is exhausted. Used to walk the separate params of a semicolon-form
/// extended color (`38;5;n` / `38;2;r;g;b`).
fn next_val<'a>(iter: &mut impl Iterator<Item = &'a [u16]>) -> Option<u16> {
    iter.next().and_then(|p| p.first().copied())
}

/// Parse a `38` (foreground) or `48` (background) extended-color payload into a
/// [`Color`], for whichever of the two wire forms `vte` produced:
///
/// - **colon** — `38:5:n` / `38:2:r:g:b`: the selector and values are
///   subparameters grouped into the single `first` slice (`first[0]` is the
///   `38`/`48`), so everything is read from `first`.
/// - **semicolon** — `38;5;n` / `38;2;r;g;b`: the selector and values are
///   separate following parameters, pulled in turn from `iter`.
///
/// Selector `5` is a 256-color palette index; selector `2` is 24-bit RGB. A
/// missing or unrecognized payload — or an out-of-range value (a palette index
/// or channel > 255) — yields `None`, leaving the pen unchanged.
fn extended_color<'a>(first: &[u16], iter: &mut impl Iterator<Item = &'a [u16]>) -> Option<Color> {
    if first.len() > 1 {
        // Colon form: selector at first[1], its values follow in the same slice.
        match first.get(1).copied()? {
            // `38:5:n` — 256-palette index is the final subparameter. Reading
            // the last (not `first[2]`) skips a leading empty colorspace slot in
            // the malformed `38:5::n` form (which `vte` stores as a `0`), keeping
            // the index symmetric with the RGB branch below; `len >= 3` requires
            // the index to be present so `38:5` alone rejects. An index that does
            // not fit a u8 (> 255) is out of range, so reject the color (`None`,
            // pen unchanged), matching vte's own `ansi.rs` (`u8::try_from(..).ok()?`).
            5 if first.len() >= 3 => Some(Color::Indexed(u8::try_from(*first.last()?).ok()?)),
            2 => {
                // The colon RGB form may carry a leading colorspace id
                // (`38:2::r:g:b`, whose empty field `vte` stores as `0`), so the
                // real r, g, b are always the last three subparameters.
                let vals = &first[2..];
                let rgb = if vals.len() >= 4 {
                    &vals[vals.len() - 3..]
                } else {
                    vals
                };
                // A channel that does not fit a u8 (> 255) is out of range →
                // reject the whole color, as vte's `ansi.rs` does.
                Some(Color::Rgb(
                    u8::try_from(*rgb.first()?).ok()?,
                    u8::try_from(*rgb.get(1)?).ok()?,
                    u8::try_from(*rgb.get(2)?).ok()?,
                ))
            }
            _ => None,
        }
    } else {
        // Semicolon form: selector then values are the next separate params.
        match next_val(iter)? {
            // `38;5;n` — one following param is the 256-palette index; reject an
            // out-of-range (> 255) index, matching vte's `ansi.rs`.
            5 => Some(Color::Indexed(u8::try_from(next_val(iter)?).ok()?)),
            // `38;2;r;g;b` — three following params are the RGB channels.
            // Consume all THREE before validating: a malformed (out-of-range)
            // channel must still drain its g/b params, or the leftover values
            // bleed back into the outer SGR loop as standalone color codes (e.g.
            // `38;2;999;31;32m` would set fg-red then fg-green). Once drained, a
            // channel > 255 rejects the whole color (`None`, pen unchanged).
            2 => {
                let (r, g, b) = (next_val(iter)?, next_val(iter)?, next_val(iter)?);
                Some(Color::Rgb(
                    u8::try_from(r).ok()?,
                    u8::try_from(g).ok()?,
                    u8::try_from(b).ok()?,
                ))
            }
            _ => None,
        }
    }
}
