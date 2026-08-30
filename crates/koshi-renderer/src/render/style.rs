//! Theme-derived [`Style`]s for the chrome: tab bar, borders, stack
//! headers, and overlays.

use super::*;

#[cfg(test)]
mod tests;

/// A tab's `#N` block. The active tab takes its ramp stop as bold text and
/// sets no background, so the bar background shows through; an inactive tab
/// paints the dimmed stop as the block background with quiet text.
pub(super) fn tab_index_style(theme: &Theme, active: bool, index: usize, count: usize) -> Style {
    if active {
        Style::default()
            .fg(theme.ramp(index, count))
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(theme.on_ramp_dim)
            .bg(theme.ramp_dim(index, count))
    }
}

/// A tab's name block: the same split as the `#N` block, without its bold.
/// The active tab's name is its ramp stop as text over the bar background; an
/// inactive tab's sits on the dimmed stop.
pub(super) fn tab_name_style(theme: &Theme, active: bool, index: usize, count: usize) -> Style {
    if active {
        Style::default().fg(theme.ramp(index, count))
    } else {
        Style::default()
            .fg(theme.on_ramp_dim)
            .bg(theme.ramp_dim(index, count))
    }
}

/// The session name anchoring the tabline's left edge: the ramp's start end
/// as bold text over the bar background.
pub(super) fn session_style(theme: &Theme) -> Style {
    Style::default()
        .fg(theme.ramp(0, 2))
        .add_modifier(Modifier::BOLD)
}

/// The `[v0.1.0]` badge beside the session name: the ramp's start end again,
/// without the name's bold.
pub(super) fn version_style(theme: &Theme) -> Style {
    Style::default().fg(theme.ramp(0, 2))
}

/// The `◀`/`▶` scroll arrows framing a scrolled tab strip: the dimmed-ramp
/// text color, in bold.
pub(super) fn scroll_arrow_style(theme: &Theme) -> Style {
    Style::default()
        .fg(theme.on_ramp_dim)
        .add_modifier(Modifier::BOLD)
}

/// The background filling a koshi-owned bar row whole — the tab bar and the
/// key-hint bar — laid down before any text.
pub(crate) fn bar_style(theme: &Theme) -> Style {
    Style::default().bg(theme.bar_bg)
}

/// The filled strip marking a collapsed stack member's koshi-owned header: the
/// theme's stack-header text color on its stack-header background.
pub(super) fn stack_header_style(theme: &Theme) -> Style {
    Style::default()
        .fg(theme.stack_header_fg)
        .bg(theme.stack_header_bg)
}

/// The mode tag anchoring the tabline's right edge: the ramp's other end as
/// bold text over the bar background.
pub(super) fn mode_style(theme: &Theme) -> Style {
    Style::default()
        .fg(theme.ramp(1, 2))
        .add_modifier(Modifier::BOLD)
}

/// Bold style for the terminal-too-small overlay message.
pub(super) fn too_small_style() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

/// Dim backdrop style for the letterbox margin around a centered layout.
pub(super) fn letterbox_style(theme: &Theme) -> Style {
    Style::default().bg(theme.letterbox)
}

/// The focused pane's border: the theme's focused-border color, in bold.
pub(super) fn border_focused_style(theme: &Theme) -> Style {
    Style::default()
        .fg(theme.border_focused)
        .add_modifier(Modifier::BOLD)
}

/// An unfocused pane's border: the theme's unfocused-border color, no bold.
pub(super) fn border_unfocused_style(theme: &Theme) -> Style {
    Style::default().fg(theme.border_unfocused)
}

/// The border of the pane under the pointer — the wheel's target: the theme's
/// hover-border color, in bold.
pub(super) fn border_hover_style(theme: &Theme) -> Style {
    Style::default()
        .fg(theme.border_hover)
        .add_modifier(Modifier::BOLD)
}
