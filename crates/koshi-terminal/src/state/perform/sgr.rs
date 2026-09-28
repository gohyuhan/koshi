//! SGR (Select Graphic Rendition) handling — SGR is the CSI (`ESC [ …`) form
//! that sets text color, bold, underline, and other display attributes:
//! apply a `CSI … m` sequence to the pen [`Style`], including the 256-color
//! and truecolor extended selectors (also used for the underline color).

use crate::style::{Color, Style, UnderlineStyle};

/// Apply an SGR (Select Graphic Rendition, `CSI … m`) sequence to `pen_style`:
/// update the pen colors and text attributes carried by subsequently printed
/// cells. An empty `csi_parameters` resets the pen (SGR `0`); the extended-color
/// selectors `38`/`48`/`58` are parsed by [`parse_extended_color`]. An unknown code
/// changes nothing.
pub(super) fn apply_sgr(pen_style: &mut Style, csi_parameters: &vte::Params) {
    if csi_parameters.is_empty() {
        pen_style.clear_style();
        return;
    }

    let mut csi_parameter_iterator = csi_parameters.iter();
    while let Some(csi_parameter_values) = csi_parameter_iterator.next() {
        // Dispatch on the SGR code `csi_parameter_values.first()`. `vte` stores an empty
        // parameter (`CSI ;m`) as `0`, which resets.
        match csi_parameter_values.first().copied().unwrap_or(0) {
            0 => pen_style.clear_style(),    // 0: reset all attributes + colors
            1 => pen_style.set_bold(true),   // 1: bold (increased intensity)
            2 => pen_style.set_faint(true),  // 2: faint (decreased intensity)
            3 => pen_style.set_italic(true), // 3: italic
            // 4: underline. An optional `4:n` subparameter selects the style:
            // bare `4` and `4:1` are single, `4:0` cancels, `4:2`-`4:5` are
            // double/curly/dotted/dashed, any other subparameter is single.
            4 => {
                let underline_style = match csi_parameter_values.get(1).copied() {
                    Some(0) => UnderlineStyle::None,
                    Some(2) => UnderlineStyle::Double,
                    Some(3) => UnderlineStyle::Curly,
                    Some(4) => UnderlineStyle::Dotted,
                    Some(5) => UnderlineStyle::Dashed,
                    _ => UnderlineStyle::Single,
                };
                pen_style.set_underline(underline_style);
            }
            5 | 6 => pen_style.set_blinking(true), // 5/6: blink (slow/rapid → one flag)
            7 => pen_style.set_reverse(true),      // 7: reverse video (swap fg/bg)
            8 => pen_style.set_concealed(true),    // 8: conceal (hidden)
            9 => pen_style.set_strikethrough(true), // 9: crossed-out (strikethrough)
            21 => pen_style.set_underline(UnderlineStyle::Double), // 21: double underline
            // 22: normal intensity — cancels both bold (1) and faint (2).
            22 => {
                pen_style.set_bold(false);
                pen_style.set_faint(false);
            }
            23 => pen_style.set_italic(false), // 23: italic off
            24 => pen_style.set_underline(UnderlineStyle::None), // 24: not underlined (cancels 4 and 21)
            25 => pen_style.set_blinking(false),                 // 25: blink off
            27 => pen_style.set_reverse(false),                  // 27: reverse off
            28 => pen_style.set_concealed(false),                // 28: reveal (conceal off)
            29 => pen_style.set_strikethrough(false),            // 29: strikethrough off
            sgr_code @ 30..=37 => {
                pen_style.set_foreground_color(Color::Indexed((sgr_code - 30) as u8))
            } // 30-37: fg palette 0-7
            sgr_code @ 90..=97 => {
                pen_style.set_foreground_color(Color::Indexed((sgr_code - 90 + 8) as u8))
            } // 90-97: bright fg 8-15
            39 => pen_style.set_foreground_color(Color::Default), // 39: default fg
            sgr_code @ 40..=47 => {
                pen_style.set_background_color(Color::Indexed((sgr_code - 40) as u8))
            } // 40-47: bg palette 0-7
            sgr_code @ 100..=107 => {
                pen_style.set_background_color(Color::Indexed((sgr_code - 100 + 8) as u8))
            } // 100-107: bright bg 8-15
            49 => pen_style.set_background_color(Color::Default), // 49: default bg
            53 => pen_style.set_overlined(true),                 // 53: overline
            55 => pen_style.set_overlined(false),                // 55: overline off
            // 38: extended fg — 256-palette (`38;5;n`) or truecolor (`38;2;r;g;b`).
            38 => {
                if let Some(extended_color) =
                    parse_extended_color(csi_parameter_values, &mut csi_parameter_iterator)
                {
                    pen_style.set_foreground_color(extended_color);
                }
            }
            // 48: extended bg — 256-palette (`48;5;n`) or truecolor (`48;2;r;g;b`).
            48 => {
                if let Some(extended_color) =
                    parse_extended_color(csi_parameter_values, &mut csi_parameter_iterator)
                {
                    pen_style.set_background_color(extended_color);
                }
            }
            // 58: underline color — same 256-palette / truecolor forms as 38/48.
            58 => {
                if let Some(extended_color) =
                    parse_extended_color(csi_parameter_values, &mut csi_parameter_iterator)
                {
                    pen_style.set_underline_color(Some(extended_color));
                }
            }
            59 => pen_style.set_underline_color(None), // 59: default underline color
            _ => {}                                    // unknown / out-of-scope SGR code: ignore
        }
    }
}

/// The primary number of the iterator's next CSI parameter, or `None` when the
/// iterator is exhausted. Walks the separate parameters of a semicolon-form
/// extended color (`38;5;n` / `38;2;r;g;b`).
fn get_next_csi_parameter_number<'a>(
    csi_parameter_iterator: &mut impl Iterator<Item = &'a [u16]>,
) -> Option<u16> {
    csi_parameter_iterator
        .next()
        .and_then(|csi_parameter_values| csi_parameter_values.first().copied())
}

/// Parse a `38` (foreground), `48` (background), or `58` (underline color)
/// extended-color payload into a [`Color`], for whichever of the two wire
/// forms `vte` produced:
///
/// - **colon** — `38:5:n` / `38:2:r:g:b`: the selector and values are
///   subparameters grouped into the single `first_csi_parameter_values` slice
///   (`first_csi_parameter_values[0]` is the `38`/`48`/`58`), so everything is
///   read from `first_csi_parameter_values`.
/// - **semicolon** — `38;5;n` / `38;2;r;g;b`: the selector and values are
///   separate following parameters, pulled in turn from `csi_parameter_iterator`.
///
/// Selector `5` is a 256-color palette index; selector `2` is 24-bit RGB. A
/// missing or unrecognized payload — or an out-of-range value (a palette index
/// or channel > 255) — yields `None`, leaving the pen unchanged.
fn parse_extended_color<'a>(
    first_csi_parameter_values: &[u16],
    csi_parameter_iterator: &mut impl Iterator<Item = &'a [u16]>,
) -> Option<Color> {
    if first_csi_parameter_values.len() > 1 {
        // Colon form: the selector is first_csi_parameter_values[1]; its values follow in the same
        // slice.
        match first_csi_parameter_values[1] {
            // `38:5:n` and `38:5::n` (`vte` stores the empty slot as `0`): the
            // index is the last subparameter. `38:5` alone, or an index over
            // 255, yields `None`.
            5 if first_csi_parameter_values.len() >= 3 => Some(Color::Indexed(
                u8::try_from(*first_csi_parameter_values.last()?).ok()?,
            )),
            2 => {
                // `38:2:r:g:b`: the three channels are the subparameters after
                // the selector. Four or more subparameters carry a colorspace
                // slot first — `38:2::r:g:b`, `38:2:1:255:0:0:0:5` — and the
                // channels are the three after it; anything past them
                // (tolerance and its colorspace) is skipped. A channel over 255
                // yields `None`.
                let color_channel_values = &first_csi_parameter_values[2..];
                let rgb_channel_values = if color_channel_values.len() >= 4 {
                    &color_channel_values[1..4]
                } else {
                    color_channel_values
                };
                Some(Color::Rgb(
                    u8::try_from(*rgb_channel_values.first()?).ok()?,
                    u8::try_from(*rgb_channel_values.get(1)?).ok()?,
                    u8::try_from(*rgb_channel_values.get(2)?).ok()?,
                ))
            }
            _ => None,
        }
    } else {
        // Semicolon form: the selector, then its values, are the next separate params.
        match get_next_csi_parameter_number(csi_parameter_iterator)? {
            // `38;5;n`: the next param is the index; over 255 yields `None`.
            5 => Some(Color::Indexed(
                u8::try_from(get_next_csi_parameter_number(csi_parameter_iterator)?).ok()?,
            )),
            // `38;2;r;g;b`: takes all three channel params, then checks their
            // range: `38;2;999;31;32` consumes `31` and `32` and yields `None`.
            2 => {
                let (red_channel, green_channel, blue_channel) = (
                    get_next_csi_parameter_number(csi_parameter_iterator)?,
                    get_next_csi_parameter_number(csi_parameter_iterator)?,
                    get_next_csi_parameter_number(csi_parameter_iterator)?,
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
