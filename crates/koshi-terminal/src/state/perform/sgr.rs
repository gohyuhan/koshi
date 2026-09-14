//! SGR (Select Graphic Rendition) handling — SGR is the CSI (`ESC [ …`) form
//! that sets text color, bold, underline, and other display attributes:
//! apply a `CSI … m` sequence to the pen [`Style`], including the 256-color
//! and truecolor extended selectors (also used for the underline color).

use crate::style::{Color, Style, UnderlineStyle};

/// Apply an SGR (Select Graphic Rendition, `CSI … m`) sequence to `style`:
/// update the pen colors and text attributes carried by subsequently printed
/// cells. An empty `params` resets the pen (SGR `0`); the extended-color
/// selectors `38`/`48`/`58` are parsed by [`parse_extended_color`]. An unknown code
/// changes nothing.
pub(super) fn apply_sgr(style: &mut Style, params: &vte::Params) {
    if params.is_empty() {
        style.reset_style();
        return;
    }

    let mut parameter_iterator = params.iter();
    while let Some(parameter_values) = parameter_iterator.next() {
        // Dispatch on the SGR code `parameter_values.first()`. `vte` stores an empty
        // parameter (`CSI ;m`) as `0`, which resets.
        match parameter_values.first().copied().unwrap_or(0) {
            0 => style.reset_style(),    // 0: reset all attributes + colors
            1 => style.set_bold(true),   // 1: bold (increased intensity)
            2 => style.set_faint(true),  // 2: faint (decreased intensity)
            3 => style.set_italic(true), // 3: italic
            // 4: underline. An optional `4:n` subparameter selects the style:
            // bare `4` and `4:1` are single, `4:0` cancels, `4:2`-`4:5` are
            // double/curly/dotted/dashed, any other subparameter is single.
            4 => {
                let underline = match parameter_values.get(1).copied() {
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
            sgr_code @ 30..=37 => style.set_foreground_color(Color::Indexed((sgr_code - 30) as u8)), // 30-37: fg palette 0-7
            sgr_code @ 90..=97 => {
                style.set_foreground_color(Color::Indexed((sgr_code - 90 + 8) as u8))
            } // 90-97: bright fg 8-15
            39 => style.set_foreground_color(Color::Default), // 39: default fg
            sgr_code @ 40..=47 => style.set_background_color(Color::Indexed((sgr_code - 40) as u8)), // 40-47: bg palette 0-7
            sgr_code @ 100..=107 => {
                style.set_background_color(Color::Indexed((sgr_code - 100 + 8) as u8))
            } // 100-107: bright bg 8-15
            49 => style.set_background_color(Color::Default), // 49: default bg
            53 => style.set_overline(true),                   // 53: overline
            55 => style.set_overline(false),                  // 55: overline off
            // 38: extended fg — 256-palette (`38;5;n`) or truecolor (`38;2;r;g;b`).
            38 => {
                if let Some(color) = parse_extended_color(parameter_values, &mut parameter_iterator)
                {
                    style.set_foreground_color(color);
                }
            }
            // 48: extended bg — 256-palette (`48;5;n`) or truecolor (`48;2;r;g;b`).
            48 => {
                if let Some(color) = parse_extended_color(parameter_values, &mut parameter_iterator)
                {
                    style.set_background_color(color);
                }
            }
            // 58: underline color — same 256-palette / truecolor forms as 38/48.
            58 => {
                if let Some(color) = parse_extended_color(parameter_values, &mut parameter_iterator)
                {
                    style.set_underline_color(Some(color));
                }
            }
            59 => style.set_underline_color(None), // 59: default underline color
            _ => {}                                // unknown / out-of-scope SGR code: ignore
        }
    }
}

/// The primary value of the iterator's next CSI parameter, or `None` when the
/// iterator is exhausted. Walks the separate params of a semicolon-form
/// extended color (`38;5;n` / `38;2;r;g;b`).
fn get_next_parameter_value<'a>(
    parameter_iterator: &mut impl Iterator<Item = &'a [u16]>,
) -> Option<u16> {
    parameter_iterator
        .next()
        .and_then(|parameter_values| parameter_values.first().copied())
}

/// Parse a `38` (foreground), `48` (background), or `58` (underline color)
/// extended-color payload into a [`Color`], for whichever of the two wire
/// forms `vte` produced:
///
/// - **colon** — `38:5:n` / `38:2:r:g:b`: the selector and values are
///   subparameters grouped into the single `first_parameter_values` slice
///   (`first_parameter_values[0]` is the `38`/`48`/`58`), so everything is
///   read from `first_parameter_values`.
/// - **semicolon** — `38;5;n` / `38;2;r;g;b`: the selector and values are
///   separate following parameters, pulled in turn from `parameter_iterator`.
///
/// Selector `5` is a 256-color palette index; selector `2` is 24-bit RGB. A
/// missing or unrecognized payload — or an out-of-range value (a palette index
/// or channel > 255) — yields `None`, leaving the pen unchanged.
fn parse_extended_color<'a>(
    first_parameter_values: &[u16],
    parameter_iterator: &mut impl Iterator<Item = &'a [u16]>,
) -> Option<Color> {
    if first_parameter_values.len() > 1 {
        // Colon form: the selector is first_parameter_values[1]; its values follow in the same slice.
        match first_parameter_values[1] {
            // `38:5:n` and `38:5::n` (`vte` stores the empty slot as `0`): the
            // index is the last subparameter. `38:5` alone, or an index over
            // 255, yields `None`.
            5 if first_parameter_values.len() >= 3 => Some(Color::Indexed(
                u8::try_from(*first_parameter_values.last()?).ok()?,
            )),
            2 => {
                // `38:2:r:g:b`: the three channels are the subparameters after
                // the selector. Four or more subparameters carry a colorspace
                // slot first — `38:2::r:g:b`, `38:2:1:255:0:0:0:5` — and the
                // channels are the three after it; anything past them
                // (tolerance and its colorspace) is skipped. A channel over 255
                // yields `None`.
                let color_channel_values = &first_parameter_values[2..];
                let rgb = if color_channel_values.len() >= 4 {
                    &color_channel_values[1..4]
                } else {
                    color_channel_values
                };
                Some(Color::Rgb(
                    u8::try_from(*rgb.first()?).ok()?,
                    u8::try_from(*rgb.get(1)?).ok()?,
                    u8::try_from(*rgb.get(2)?).ok()?,
                ))
            }
            _ => None,
        }
    } else {
        // Semicolon form: the selector, then its values, are the next separate params.
        match get_next_parameter_value(parameter_iterator)? {
            // `38;5;n`: the next param is the index; over 255 yields `None`.
            5 => Some(Color::Indexed(
                u8::try_from(get_next_parameter_value(parameter_iterator)?).ok()?,
            )),
            // `38;2;r;g;b`: takes all three channel params, then checks their
            // range: `38;2;999;31;32` consumes `31` and `32` and yields `None`.
            2 => {
                let (red_channel, green_channel, blue_channel) = (
                    get_next_parameter_value(parameter_iterator)?,
                    get_next_parameter_value(parameter_iterator)?,
                    get_next_parameter_value(parameter_iterator)?,
                );
                Some(Color::Rgb(
                    u8::try_from(red_channel).ok()?,
                    u8::try_from(green_channel).ok()?,
                    u8::try_from(blue_channel).ok()?,
                ))
            }
            _ => None,
        }
    }
}
