//! The resolved chrome theme: every color the renderer paints koshi-owned
//! surfaces with. The viewer passes it to the renderer next to the frame
//! snapshot; the snapshot itself carries no colors.
//!
//! Chrome elements that come in runs — the tab list, the statusline's modifier
//! groups — each take one stop on a gradient by their position.
//! [`Theme::get_ramp_color`] gives a run element its stop; [`Theme::get_dimmed_ramp_color`] is the
//! same stop pulled toward black, used as the quiet half of a two-block ribbon
//! (label next to key, for example). The single accent for in-progress state
//! (the pending-sequence breadcrumb) is [`Theme::accent_color`]. Both koshi-owned
//! rows — the tabline and the statusline — are filled with
//! [`Theme::bar_background_color`]
//! before anything is painted over them. [`Theme::compute_client_color`] gives
//! each attached client the color its chip and its cursor marker draw in.
//! [`Theme::default`] is the stock koshi look — a light-purple → light-blue
//! ramp with a pink accent over black bars;
//! the viewing client builds a non-default `Theme` from the config theme's
//! palette, where `ramp_start "#ff0000"` turns the first tab's ribbon red.

use ratatui::style::Color;

/// The hue step between two clients whose indexes differ by one, in degrees:
/// 360° × (2 − φ), where φ is the golden ratio.
const GOLDEN_ANGLE_DEGREES: f64 = 137.507_764_050_037_85;

/// The contrast ratio against the bar background that a client's fill color
/// reaches at least.
const MINIMUM_CLIENT_FILL_CONTRAST_RATIO: f64 = 3.0;

/// The colors one attached client draws in: the number chip in the tab strip
/// and the cursor marker in a pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientColor {
    /// The background of the chip and of the marker cell.
    pub fill_color: Color,
    /// The text over `fill_color`: black `#000000` or white `#ffffff`.
    pub text_color: Color,
}

/// Every color the renderer's chrome styles draw with. The style helper
/// functions in [`crate::render`] and the statusline module are the
/// only places chrome picks a color, and each reads its colors from here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    /// First endpoint of the chrome gradient, as `(r, g, b)` channels, taken
    /// whole by the first element of a run.
    pub ramp_start: (u8, u8, u8),
    /// Second endpoint of the chrome gradient, as `(r, g, b)` channels,
    /// taken whole by the last element of a run.
    pub ramp_end: (u8, u8, u8),
    /// Text color over a ramp-colored block.
    pub ramp_block_text_color: Color,
    /// Text color over a dimmed ramp block.
    pub dimmed_ramp_text_color: Color,
    /// The in-progress accent, brighter than any ramp stop: marks the chords
    /// already pressed in a pending key sequence.
    pub accent_color: Color,
    /// Text color over an accent block.
    pub accent_block_text_color: Color,
    /// Border of the focused pane.
    pub focused_border_color: Color,
    /// Border of unfocused panes.
    pub unfocused_border_color: Color,
    /// Border of the pane the pointer is hovering over — the pane the wheel
    /// scrolls.
    pub hover_border_color: Color,
    /// Text of a collapsed stack member's header strip.
    pub stack_header_text_color: Color,
    /// Background of a collapsed stack member's header strip.
    pub stack_header_background_color: Color,
    /// Backdrop of the letterbox margin around a centered layout.
    pub letterbox_color: Color,
    /// Background filling koshi's own two rows whole: the tab bar on top and
    /// the statusline on the bottom, as `(r, g, b)` channels.
    pub bar_background_color: (u8, u8, u8),
}

impl Default for Theme {
    /// The stock koshi chrome: a light-purple → light-blue ramp with a pink
    /// accent over black bars. Field-for-field the same colors as
    /// `koshi_config::types::ColorPalette::default`: an unthemed frame and a
    /// default-config frame paint identically.
    fn default() -> Self {
        Self {
            ramp_start: (0xd0, 0xa5, 0xff),
            ramp_end: (0x7d, 0xbc, 0xff),
            ramp_block_text_color: Color::Rgb(0x12, 0x09, 0x1f),
            dimmed_ramp_text_color: Color::Rgb(0xf0, 0xec, 0xfa),
            accent_color: Color::Rgb(0xf5, 0xc2, 0xff),
            accent_block_text_color: Color::Rgb(0x1e, 0x10, 0x33),
            focused_border_color: Color::Rgb(0x00, 0xaf, 0xd7),
            unfocused_border_color: Color::Rgb(0x58, 0x58, 0x58),
            hover_border_color: Color::Rgb(0xaf, 0x5f, 0xff),
            stack_header_text_color: Color::Rgb(0xf4, 0xf1, 0xfa),
            stack_header_background_color: Color::Rgb(0x30, 0x0f, 0x4a),
            letterbox_color: Color::Rgb(0x58, 0x58, 0x58),
            bar_background_color: (0x00, 0x00, 0x00),
        }
    }
}

impl Theme {
    /// The ramp color for element `ramp_element_index` of a
    /// `ramp_element_count`-element run: `0` is the
    /// [`ramp_start`](Theme::ramp_start) end, `ramp_element_count - 1` the
    /// [`ramp_end`](Theme::ramp_end) end. A run of one takes the start end
    /// whole.
    #[must_use]
    pub fn get_ramp_color(&self, ramp_element_index: usize, ramp_element_count: usize) -> Color {
        let (red_channel, green_channel, blue_channel) =
            self.compute_ramp_rgb(ramp_element_index, ramp_element_count);
        Color::Rgb(red_channel, green_channel, blue_channel)
    }

    /// The same ramp color pulled 45% toward black: the quiet background
    /// paired with a [`get_ramp_color`](Theme::get_ramp_color)-colored block.
    #[must_use]
    pub fn get_dimmed_ramp_color(
        &self,
        ramp_element_index: usize,
        ramp_element_count: usize,
    ) -> Color {
        let (red_channel, green_channel, blue_channel) =
            self.compute_ramp_rgb(ramp_element_index, ramp_element_count);
        Color::Rgb(
            scale_color_channel(red_channel, 55),
            scale_color_channel(green_channel, 55),
            scale_color_channel(blue_channel, 55),
        )
    }

    /// The colors of the client whose index is `color_index`.
    ///
    /// The fill color is an HSL color at full saturation. Its hue is
    /// `color_index` times 137.508° (the golden angle), modulo 360°. Its
    /// lightness is the first of 50%, 51%, … 99% that has a contrast ratio of
    /// 3:1 or more against [`bar_background_color`](Theme::bar_background_color),
    /// and 100% (white) when none has. On a bar that has a higher contrast
    /// ratio against black than against white, the steps go down instead:
    /// 50%, 49%, … 1%, then 0% (black). The text is black or white, whichever
    /// has the higher contrast ratio against the fill, which is 4.58:1 at
    /// least. A contrast ratio is `(L1 + 0.05) / (L2 + 0.05)` over the WCAG
    /// relative luminances of the lighter and the darker color.
    ///
    /// Example: over the default black bar, index `0` is `#ff0000` with
    /// black text, index `2` is `#9500ff` with white text, and over the bar
    /// `#eff1f5`, index `1` is `#009e2e` with black text.
    #[must_use]
    pub fn compute_client_color(&self, color_index: u32) -> ClientColor {
        let hue_degrees = (f64::from(color_index) * GOLDEN_ANGLE_DEGREES) % 360.0;
        let bar_luminance = compute_relative_luminance(self.bar_background_color);
        let is_white_side_legible = compute_contrast_ratio(1.0, bar_luminance)
            >= compute_contrast_ratio(0.0, bar_luminance);
        let fill_rgb = (0..50_u8)
            .map(|lightness_step| {
                if is_white_side_legible {
                    50 + lightness_step
                } else {
                    50 - lightness_step
                }
            })
            .map(|lightness_percentage| convert_hsl_to_rgb(hue_degrees, lightness_percentage))
            .find(|fill_rgb| {
                compute_contrast_ratio(compute_relative_luminance(*fill_rgb), bar_luminance)
                    >= MINIMUM_CLIENT_FILL_CONTRAST_RATIO
            })
            .unwrap_or(if is_white_side_legible {
                (0xff, 0xff, 0xff)
            } else {
                (0x00, 0x00, 0x00)
            });
        let fill_luminance = compute_relative_luminance(fill_rgb);
        let text_color = if compute_contrast_ratio(0.0, fill_luminance)
            >= compute_contrast_ratio(1.0, fill_luminance)
        {
            Color::Rgb(0x00, 0x00, 0x00)
        } else {
            Color::Rgb(0xff, 0xff, 0xff)
        };
        ClientColor {
            fill_color: Color::from(fill_rgb),
            text_color,
        }
    }

    fn compute_ramp_rgb(
        &self,
        ramp_element_index: usize,
        ramp_element_count: usize,
    ) -> (u8, u8, u8) {
        let last_ramp_element_index = ramp_element_count.saturating_sub(1);
        let clamped_ramp_element_index = ramp_element_index.min(last_ramp_element_index);
        (
            compute_interpolated_channel(
                self.ramp_start.0,
                self.ramp_end.0,
                clamped_ramp_element_index,
                last_ramp_element_index,
            ),
            compute_interpolated_channel(
                self.ramp_start.1,
                self.ramp_end.1,
                clamped_ramp_element_index,
                last_ramp_element_index,
            ),
            compute_interpolated_channel(
                self.ramp_start.2,
                self.ramp_end.2,
                clamped_ramp_element_index,
                last_ramp_element_index,
            ),
        )
    }
}

/// Integer interpolation from `start_channel` to `end_channel` at position
/// `ramp_element_index` of `last_ramp_element_index`; a one-element run
/// (`last_ramp_element_index == 0`) stays at `start_channel`.
fn compute_interpolated_channel(
    start_channel: u8,
    end_channel: u8,
    ramp_element_index: usize,
    last_ramp_element_index: usize,
) -> u8 {
    if last_ramp_element_index == 0 {
        return start_channel;
    }
    let start_channel = i128::from(start_channel);
    let end_channel = i128::from(end_channel);
    // `i128` holds every `usize` value on every build target.
    let interpolated_channel = start_channel
        + (end_channel - start_channel) * (ramp_element_index as i128)
            / (last_ramp_element_index as i128);
    interpolated_channel.clamp(0, 255) as u8
}

/// The sRGB color of hue `hue_degrees` (0 to 360), full saturation and
/// lightness `lightness_percentage` (0 to 100), by the HSL to sRGB conversion
/// of CSS Color 4, each channel rounded to the nearest of 0 to 255. Hue `0` at
/// `50` is `(255, 0, 0)`, hue `240` at `60` is `(51, 51, 255)`, and any
/// hue at `100` is `(255, 255, 255)`.
pub(crate) fn convert_hsl_to_rgb(hue_degrees: f64, lightness_percentage: u8) -> (u8, u8, u8) {
    let lightness = f64::from(lightness_percentage) / 100.0;
    let chroma_half = lightness.min(1.0 - lightness);
    let compute_channel = |channel_offset: f64| {
        let hue_position = (channel_offset + hue_degrees / 30.0) % 12.0;
        let channel_fraction = lightness
            - chroma_half
                * (hue_position - 3.0)
                    .min(9.0 - hue_position)
                    .clamp(-1.0, 1.0);
        (channel_fraction * 255.0).round() as u8
    };
    (
        compute_channel(0.0),
        compute_channel(8.0),
        compute_channel(4.0),
    )
}

/// The WCAG relative luminance of an sRGB color, from `0.0` (black) to
/// `1.0` (white): `0.2126 R + 0.7152 G + 0.0722 B` over the linear channels,
/// where a channel `c` of `0.0` to `1.0` is `c / 12.92` up to `0.04045` and
/// `((c + 0.055) / 1.055)^2.4` above it. `(255, 0, 0)` gives `0.2126`.
pub(crate) fn compute_relative_luminance(
    (red_channel, green_channel, blue_channel): (u8, u8, u8),
) -> f64 {
    let linearize_channel = |channel_value: u8| {
        let channel_fraction = f64::from(channel_value) / 255.0;
        if channel_fraction <= 0.04045 {
            channel_fraction / 12.92
        } else {
            ((channel_fraction + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linearize_channel(red_channel)
        + 0.7152 * linearize_channel(green_channel)
        + 0.0722 * linearize_channel(blue_channel)
}

/// The WCAG contrast ratio of two relative luminances, from `1.0` to `21.0`:
/// `(L1 + 0.05) / (L2 + 0.05)` with `L1` the larger. White against black
/// gives `21.0`, in either order.
pub(crate) fn compute_contrast_ratio(first_luminance: f64, second_luminance: f64) -> f64 {
    let lighter_luminance = first_luminance.max(second_luminance);
    let darker_luminance = first_luminance.min(second_luminance);
    (lighter_luminance + 0.05) / (darker_luminance + 0.05)
}

/// `channel_value` scaled to `percentage` of itself.
fn scale_color_channel(channel_value: u8, percentage: u16) -> u8 {
    ((u16::from(channel_value) * percentage) / 100) as u8
}

#[cfg(test)]
mod tests;
