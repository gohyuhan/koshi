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
//! before anything is painted over them. [`Theme::default`] is the stock koshi
//! look — a light-purple → light-blue ramp with a pink accent over black bars;
//! the viewing client builds a non-default `Theme` from the config theme's
//! palette, where `ramp_start "#ff0000"` turns the first tab's ribbon red.

use ratatui::style::Color;

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
    /// the statusline on the bottom.
    pub bar_background_color: Color,
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
            bar_background_color: Color::Rgb(0x00, 0x00, 0x00),
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
    // `i128` holds every `usize` on every target this builds for, so a long
    // run never wraps its denominator negative and flips the interpolation.
    let interpolated_channel = start_channel
        + (end_channel - start_channel) * (ramp_element_index as i128)
            / (last_ramp_element_index as i128);
    interpolated_channel.clamp(0, 255) as u8
}

/// `channel_value` scaled to `percentage` of itself.
fn scale_color_channel(channel_value: u8, percentage: u16) -> u8 {
    ((u16::from(channel_value) * percentage) / 100) as u8
}

#[cfg(test)]
mod tests;
