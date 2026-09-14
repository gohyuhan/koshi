//! Theme-derived [`Style`]s for the chrome: tab bar, borders, stack
//! headers, and overlays.

use super::*;

#[cfg(test)]
mod tests;

/// A tab's `#N` block. The active tab takes its ramp stop as bold text and
/// sets no background, so the bar background shows through; an inactive tab
/// paints the dimmed stop as the block background with quiet text.
pub(super) fn compute_tab_index_style(
    theme: &Theme,
    is_active: bool,
    tab_index: usize,
    tab_count: usize,
) -> Style {
    if is_active {
        Style::default()
            .fg(theme.get_ramp_color(tab_index, tab_count))
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(theme.dimmed_ramp_text_color)
            .bg(theme.get_dimmed_ramp_color(tab_index, tab_count))
    }
}

/// A tab's name block: the same split as the `#N` block, without its bold.
/// The active tab's name is its ramp stop as text over the bar background; an
/// inactive tab's sits on the dimmed stop.
pub(super) fn compute_tab_name_style(
    theme: &Theme,
    is_active: bool,
    tab_index: usize,
    tab_count: usize,
) -> Style {
    if is_active {
        Style::default().fg(theme.get_ramp_color(tab_index, tab_count))
    } else {
        Style::default()
            .fg(theme.dimmed_ramp_text_color)
            .bg(theme.get_dimmed_ramp_color(tab_index, tab_count))
    }
}

/// The session name anchoring the tabline's left edge: the ramp's start end
/// as bold text over the bar background.
pub(super) fn compute_session_style(theme: &Theme) -> Style {
    Style::default()
        .fg(theme.get_ramp_color(0, 2))
        .add_modifier(Modifier::BOLD)
}

/// The `[v0.1.0]` badge beside the session name: the ramp's start end again,
/// without the name's bold.
pub(super) fn compute_version_badge_style(theme: &Theme) -> Style {
    Style::default().fg(theme.get_ramp_color(0, 2))
}

/// The `◀`/`▶` scroll arrows framing a scrolled tab strip: the dimmed-ramp
/// text color, in bold.
pub(super) fn compute_scroll_arrow_style(theme: &Theme) -> Style {
    Style::default()
        .fg(theme.dimmed_ramp_text_color)
        .add_modifier(Modifier::BOLD)
}

/// The background filling a koshi-owned bar row whole — the tab bar and the
/// statusline — laid down before any text.
pub(crate) fn compute_bar_style(theme: &Theme) -> Style {
    Style::default().bg(theme.bar_background_color)
}

/// The filled strip marking a collapsed stack member's koshi-owned header: the
/// theme's stack-header text color on its stack-header background.
pub(super) fn compute_stack_header_style(theme: &Theme) -> Style {
    Style::default()
        .fg(theme.stack_header_text_color)
        .bg(theme.stack_header_background_color)
}

/// The mode tag anchoring the tabline's right edge: the ramp's other end as
/// bold text over the bar background.
pub(super) fn compute_mode_style(theme: &Theme) -> Style {
    Style::default()
        .fg(theme.get_ramp_color(1, 2))
        .add_modifier(Modifier::BOLD)
}

/// Bold style for the terminal-too-small overlay message.
pub(super) fn compute_too_small_overlay_style() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

/// Dim backdrop style for the letterbox margin around a centered layout.
pub(super) fn compute_letterbox_style(theme: &Theme) -> Style {
    Style::default().bg(theme.letterbox_color)
}

/// The focused pane's border: the theme's focused-border color, in bold.
pub(super) fn compute_focused_border_style(theme: &Theme) -> Style {
    Style::default()
        .fg(theme.focused_border_color)
        .add_modifier(Modifier::BOLD)
}

/// An unfocused pane's border: the theme's unfocused-border color, no bold.
pub(super) fn compute_unfocused_border_style(theme: &Theme) -> Style {
    Style::default().fg(theme.unfocused_border_color)
}

/// The border of the pane under the pointer — the wheel's target: the theme's
/// hover-border color, in bold.
pub(super) fn compute_hover_border_style(theme: &Theme) -> Style {
    Style::default()
        .fg(theme.hover_border_color)
        .add_modifier(Modifier::BOLD)
}
