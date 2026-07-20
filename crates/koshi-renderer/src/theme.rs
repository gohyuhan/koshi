//! The resolved chrome theme: every color the renderer paints koshi-owned
//! surfaces with, carried on each frame's snapshot.
//!
//! Chrome elements that come in runs — the tab list, the hint bar's modifier
//! groups — each take one stop on a gradient by their position, so a frame
//! reads as one gradient rather than a scatter of colors. [`Theme::ramp`]
//! gives a run element its stop; [`Theme::ramp_dim`] is the same stop pulled
//! toward black, used as the quiet half of a two-block ribbon (label next to
//! key, for example). The single accent for in-progress state (the
//! pending-sequence breadcrumb) is [`Theme::accent`]. Both koshi-owned rows —
//! the tab bar and the key-hint bar — are filled with [`Theme::bar_bg`] before
//! anything is painted over them, so chrome text reads against a known
//! background rather than whatever the terminal's own is. [`Theme::default`] is
//! the stock koshi look — a light-purple → light-blue ramp with a pink accent
//! over black bars;
//! the runtime builds a non-default `Theme` from the config theme's palette,
//! so `ramp_start "#ff0000"` in a theme turns the first tab's ribbon red.

use ratatui::style::Color;

/// Every color the renderer's chrome styles draw with. The style helper
/// functions in [`crate::render`] and [`crate::statusline_hints`] are the
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
    pub on_ramp: Color,
    /// Text color over a dimmed ramp block.
    pub on_ramp_dim: Color,
    /// The in-progress accent, brighter than any ramp stop: marks the chords
    /// already pressed in a pending key sequence.
    pub accent: Color,
    /// Text color over an accent block.
    pub on_accent: Color,
    /// Border of the focused pane.
    pub border_focused: Color,
    /// Border of unfocused panes.
    pub border_unfocused: Color,
    /// Border of the pane the pointer is hovering over — the pane the wheel
    /// scrolls.
    pub border_hover: Color,
    /// Text of a collapsed stack member's header strip.
    pub stack_header_fg: Color,
    /// Background of a collapsed stack member's header strip.
    pub stack_header_bg: Color,
    /// Backdrop of the letterbox margin around a centered layout.
    pub letterbox: Color,
    /// Background filling koshi's own two rows whole: the tab bar on top and
    /// the key-hint bar on the bottom.
    pub bar_bg: Color,
}

impl Default for Theme {
    /// The stock koshi chrome: a light-purple → light-blue ramp with a pink
    /// accent over black bars. Field-for-field the same colors as the config
    /// crate's default palette, so an unthemed frame and a default-config
    /// frame paint identically.
    fn default() -> Self {
        Self {
            ramp_start: (0xd0, 0xa5, 0xff),
            ramp_end: (0x7d, 0xbc, 0xff),
            on_ramp: Color::Rgb(0x12, 0x09, 0x1f),
            on_ramp_dim: Color::Rgb(0xf0, 0xec, 0xfa),
            accent: Color::Rgb(0xf5, 0xc2, 0xff),
            on_accent: Color::Rgb(0x1e, 0x10, 0x33),
            border_focused: Color::Rgb(0x00, 0xaf, 0xd7),
            border_unfocused: Color::Rgb(0x58, 0x58, 0x58),
            border_hover: Color::Rgb(0xaf, 0x5f, 0xff),
            stack_header_fg: Color::Rgb(0xf4, 0xf1, 0xfa),
            stack_header_bg: Color::Rgb(0x30, 0x0f, 0x4a),
            letterbox: Color::Rgb(0x58, 0x58, 0x58),
            bar_bg: Color::Rgb(0x00, 0x00, 0x00),
        }
    }
}

impl Theme {
    /// The ramp stop for element `index` of a `count`-element run: `0` is
    /// the [`ramp_start`](Theme::ramp_start) end, `count - 1` the
    /// [`ramp_end`](Theme::ramp_end) end. A run of one takes the start end
    /// whole.
    #[must_use]
    pub fn ramp(&self, index: usize, count: usize) -> Color {
        let (r, g, b) = self.ramp_rgb(index, count);
        Color::Rgb(r, g, b)
    }

    /// The same ramp stop pulled 45% toward black: the quiet background
    /// paired with a [`ramp`](Theme::ramp)-colored block.
    #[must_use]
    pub fn ramp_dim(&self, index: usize, count: usize) -> Color {
        let (r, g, b) = self.ramp_rgb(index, count);
        Color::Rgb(scale(r, 55), scale(g, 55), scale(b, 55))
    }

    fn ramp_rgb(&self, index: usize, count: usize) -> (u8, u8, u8) {
        let last = count.saturating_sub(1);
        let index = index.min(last);
        (
            lerp(self.ramp_start.0, self.ramp_end.0, index, last),
            lerp(self.ramp_start.1, self.ramp_end.1, index, last),
            lerp(self.ramp_start.2, self.ramp_end.2, index, last),
        )
    }
}

/// Integer interpolation from `a` to `b` at position `num` of `den`; a run of
/// one element (`den == 0`) sits at `a`.
fn lerp(a: u8, b: u8, num: usize, den: usize) -> u8 {
    if den == 0 {
        return a;
    }
    let a = i32::from(a);
    let b = i32::from(b);
    let mixed = a + (b - a) * (num as i32) / (den as i32);
    mixed.clamp(0, 255) as u8
}

/// `value` scaled to `percent` of itself.
fn scale(value: u8, percent: u16) -> u8 {
    ((u16::from(value) * percent) / 100) as u8
}

#[cfg(test)]
mod tests;
