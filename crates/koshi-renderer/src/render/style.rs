//! Theme-derived [`Style`]s for the chrome: tab bar, borders, stack
//! headers, and overlays.

use super::*;

/// A tab's `#N` block. The active tab is inverted — its ramp stop as the
/// TEXT color on the terminal's own background; an inactive tab paints the
/// dimmed stop as the block background with quiet text.
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

/// A tab's name block: same inversion as the `#N` block — the active tab's
/// name is its ramp stop as text on the terminal background, an inactive
/// tab's sits on the dimmed stop.
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
/// as the text color on the terminal's own background.
pub(super) fn session_style(theme: &Theme) -> Style {
    Style::default()
        .fg(theme.ramp(0, 2))
        .add_modifier(Modifier::BOLD)
}

/// The `<`/`>` scroll arrows framing a scrolled tab strip.
pub(super) fn scroll_arrow_style(theme: &Theme) -> Style {
    Style::default()
        .fg(theme.on_ramp_dim)
        .add_modifier(Modifier::BOLD)
}

/// Filled strip style marking a collapsed stack member's koshi-owned header.
pub(super) fn stack_header_style(theme: &Theme) -> Style {
    Style::default()
        .fg(theme.stack_header_fg)
        .bg(theme.stack_header_bg)
}

/// The mode tag anchoring the tabline's right edge: the ramp's other end as
/// the text color on the terminal's own background.
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

/// Highlighted border style for the focused pane.
pub(super) fn border_focused_style(theme: &Theme) -> Style {
    Style::default()
        .fg(theme.border_focused)
        .add_modifier(Modifier::BOLD)
}

/// Dim border style for unfocused panes.
pub(super) fn border_unfocused_style(theme: &Theme) -> Style {
    Style::default().fg(theme.border_unfocused)
}

/// Border style for the pane under the pointer — the wheel's target.
pub(super) fn border_hover_style(theme: &Theme) -> Style {
    Style::default()
        .fg(theme.border_hover)
        .add_modifier(Modifier::BOLD)
}
