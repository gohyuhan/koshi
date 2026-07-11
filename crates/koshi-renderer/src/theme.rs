//! The koshi chrome palette: a dark-purple → blue ramp shared by every
//! koshi-owned surface.
//!
//! Chrome elements that come in runs — the tab list, the hint bar's modifier
//! groups — each take one stop on the ramp by their position, so a frame
//! reads as one gradient rather than a scatter of colors. [`ramp`] gives a
//! run element its stop; [`ramp_dim`] is the same stop pulled toward black,
//! used as the quiet half of a two-block ribbon (label next to key, for
//! example). The single accent for in-progress state (the pending-sequence
//! breadcrumb) is [`ACCENT`]. These constants are the theme seam: the
//! config-driven theme task swaps their sources without touching callers.

use ratatui::style::Color;

/// The dark-purple end of the ramp, taken by the first element of a run.
const RAMP_START: (u8, u8, u8) = (0x58, 0x1c, 0x87);

/// The blue end of the ramp, taken by the last element of a run.
const RAMP_END: (u8, u8, u8) = (0x3b, 0x82, 0xf6);

/// Text color over a ramp-colored block.
pub(crate) const ON_RAMP: Color = Color::Rgb(0xf4, 0xf1, 0xfa);

/// Text color over a dimmed ramp block.
pub(crate) const ON_RAMP_DIM: Color = Color::Rgb(0xc9, 0xc4, 0xd4);

/// The in-progress accent (violet), brighter than any ramp stop: marks the
/// chords already pressed in a pending key sequence.
pub(crate) const ACCENT: Color = Color::Rgb(0xa7, 0x8b, 0xfa);

/// Text color over an accent block.
pub(crate) const ON_ACCENT: Color = Color::Rgb(0x1e, 0x10, 0x33);

/// The ramp stop for element `index` of a `count`-element run: `0` is the
/// dark-purple end, `count - 1` the blue end. A run of one takes the purple
/// end whole.
pub(crate) fn ramp(index: usize, count: usize) -> Color {
    let (r, g, b) = ramp_rgb(index, count);
    Color::Rgb(r, g, b)
}

/// The same ramp stop pulled 45% toward black: the quiet background paired
/// with a [`ramp`]-colored block.
pub(crate) fn ramp_dim(index: usize, count: usize) -> Color {
    let (r, g, b) = ramp_rgb(index, count);
    Color::Rgb(scale(r, 55), scale(g, 55), scale(b, 55))
}

fn ramp_rgb(index: usize, count: usize) -> (u8, u8, u8) {
    let last = count.saturating_sub(1);
    let index = index.min(last);
    (
        lerp(RAMP_START.0, RAMP_END.0, index, last),
        lerp(RAMP_START.1, RAMP_END.1, index, last),
        lerp(RAMP_START.2, RAMP_END.2, index, last),
    )
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
