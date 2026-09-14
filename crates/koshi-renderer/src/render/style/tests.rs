//! Tests for the chrome style helpers: each one turns a theme into an exact
//! ratatui `Style`, the tab styles invert between the active and inactive tab,
//! and a run position past the last stop clamps to it.

use super::*;

/// The stock theme most cases below read their colors from.
fn build_default_theme() -> Theme {
    Theme::default()
}

#[test]
fn active_tab_index_is_its_ramp_stop_as_bold_text() {
    // Active: the ramp stop is the TEXT color, no block background.
    let active_tab_index_style = compute_tab_index_style(&build_default_theme(), true, 0, 1);
    let expected_tab_index_style = Style::default()
        .fg(Color::Rgb(0xd0, 0xa5, 0xff))
        .add_modifier(Modifier::BOLD);
    assert_eq!(active_tab_index_style, expected_tab_index_style);
}

#[test]
fn inactive_tab_index_is_quiet_text_on_the_dimmed_stop() {
    // Inactive: quiet text over the dimmed ramp stop as the block background.
    let inactive_tab_index_style = compute_tab_index_style(&build_default_theme(), false, 0, 1);
    let expected_tab_index_style = Style::default()
        .fg(Color::Rgb(0xf0, 0xec, 0xfa))
        .bg(Color::Rgb(0x72, 0x5a, 0x8c));
    assert_eq!(inactive_tab_index_style, expected_tab_index_style);
}

#[test]
fn active_tab_name_is_its_ramp_stop_without_bold() {
    // The name block takes the same inversion as the `#N` block but is not bold.
    let active_tab_name_style = compute_tab_name_style(&build_default_theme(), true, 0, 1);
    let expected_tab_name_style = Style::default().fg(Color::Rgb(0xd0, 0xa5, 0xff));
    assert_eq!(active_tab_name_style, expected_tab_name_style);
}

#[test]
fn inactive_tab_name_matches_the_inactive_index_block() {
    let inactive_tab_name_style = compute_tab_name_style(&build_default_theme(), false, 0, 1);
    let expected_tab_name_style = Style::default()
        .fg(Color::Rgb(0xf0, 0xec, 0xfa))
        .bg(Color::Rgb(0x72, 0x5a, 0x8c));
    assert_eq!(inactive_tab_name_style, expected_tab_name_style);
}

#[test]
fn a_middle_tab_stop_reads_the_blended_ramp_color() {
    // The style helper passes index/count straight to `Theme::ramp`, so tab 1
    // of 3 gets the blended middle stop, not an endpoint.
    let middle_tab_index_style = compute_tab_index_style(&build_default_theme(), true, 1, 3);
    let expected_tab_index_style = Style::default()
        .fg(Color::Rgb(0xa7, 0xb0, 0xff))
        .add_modifier(Modifier::BOLD);
    assert_eq!(middle_tab_index_style, expected_tab_index_style);
}

#[test]
fn the_last_tab_stop_is_the_ramp_far_end() {
    // Tab 2 of 3 is the run's last element, so it takes `ramp_end` whole.
    let last_tab_index_style = compute_tab_index_style(&build_default_theme(), true, 2, 3);
    let expected_tab_index_style = Style::default()
        .fg(Color::Rgb(0x7d, 0xbc, 0xff))
        .add_modifier(Modifier::BOLD);
    assert_eq!(last_tab_index_style, expected_tab_index_style);
}

#[test]
fn a_tab_stop_past_the_end_of_the_run_clamps_to_the_last() {
    // Index 5 in a run of 3 reads the same stop as index 2.
    let clamped_tab_index_style = compute_tab_index_style(&build_default_theme(), true, 5, 3);
    let expected_tab_index_style = Style::default()
        .fg(Color::Rgb(0x7d, 0xbc, 0xff))
        .add_modifier(Modifier::BOLD);
    assert_eq!(clamped_tab_index_style, expected_tab_index_style);
}

#[test]
fn an_empty_run_reads_the_ramp_start_end() {
    // A count of 0 has no last stop; the run sits at `ramp_start`.
    let empty_run_tab_index_style = compute_tab_index_style(&build_default_theme(), true, 0, 0);
    let expected_tab_index_style = Style::default()
        .fg(Color::Rgb(0xd0, 0xa5, 0xff))
        .add_modifier(Modifier::BOLD);
    assert_eq!(empty_run_tab_index_style, expected_tab_index_style);
}

#[test]
fn an_inactive_last_tab_sits_on_the_dimmed_far_end() {
    // The dimmed stop is the far end pulled 45% toward black.
    let inactive_last_tab_name_style = compute_tab_name_style(&build_default_theme(), false, 2, 3);
    let expected_tab_name_style = Style::default()
        .fg(Color::Rgb(0xf0, 0xec, 0xfa))
        .bg(Color::Rgb(0x44, 0x67, 0x8c));
    assert_eq!(inactive_last_tab_name_style, expected_tab_name_style);
}

#[test]
fn a_custom_ramp_recolors_the_active_tab() {
    // The stops come from the theme's own endpoints, not from fixed colors.
    let custom_theme = Theme {
        ramp_start: (0x10, 0x20, 0x30),
        ramp_end: (0x40, 0x50, 0x60),
        ..Theme::default()
    };
    let custom_tab_index_style = compute_tab_index_style(&custom_theme, true, 1, 2);
    let expected_tab_index_style = Style::default()
        .fg(Color::Rgb(0x40, 0x50, 0x60))
        .add_modifier(Modifier::BOLD);
    assert_eq!(custom_tab_index_style, expected_tab_index_style);
}

#[test]
fn a_custom_bar_background_recolors_the_bar_fill() {
    let custom_theme = Theme {
        bar_background_color: Color::Rgb(0x01, 0x02, 0x03),
        ..Theme::default()
    };
    let custom_bar_style = compute_bar_style(&custom_theme);
    let expected_bar_style = Style::default().bg(Color::Rgb(0x01, 0x02, 0x03));
    assert_eq!(custom_bar_style, expected_bar_style);
}

#[test]
fn session_block_is_the_ramp_start_end_as_bold_text() {
    let session_name_style = compute_session_style(&build_default_theme());
    let expected_session_name_style = Style::default()
        .fg(Color::Rgb(0xd0, 0xa5, 0xff))
        .add_modifier(Modifier::BOLD);
    assert_eq!(session_name_style, expected_session_name_style);
}

#[test]
fn version_badge_is_the_ramp_start_end_without_bold() {
    let version_badge_style = compute_version_badge_style(&build_default_theme());
    let expected_version_badge_style = Style::default().fg(Color::Rgb(0xd0, 0xa5, 0xff));
    assert_eq!(version_badge_style, expected_version_badge_style);
}

#[test]
fn mode_tag_is_the_ramp_far_end_as_bold_text() {
    let mode_tag_style = compute_mode_style(&build_default_theme());
    let expected_mode_tag_style = Style::default()
        .fg(Color::Rgb(0x7d, 0xbc, 0xff))
        .add_modifier(Modifier::BOLD);
    assert_eq!(mode_tag_style, expected_mode_tag_style);
}

#[test]
fn scroll_arrow_is_quiet_text_in_bold() {
    let scroll_arrow_style = compute_scroll_arrow_style(&build_default_theme());
    let expected_scroll_arrow_style = Style::default()
        .fg(Color::Rgb(0xf0, 0xec, 0xfa))
        .add_modifier(Modifier::BOLD);
    assert_eq!(scroll_arrow_style, expected_scroll_arrow_style);
}

#[test]
fn bar_sets_only_the_row_background() {
    let bar_background_style = compute_bar_style(&build_default_theme());
    let expected_bar_background_style = Style::default().bg(Color::Rgb(0x00, 0x00, 0x00));
    assert_eq!(bar_background_style, expected_bar_background_style);
}

#[test]
fn stack_header_uses_its_own_two_colors() {
    let stack_header_style = compute_stack_header_style(&build_default_theme());
    let expected_stack_header_style = Style::default()
        .fg(Color::Rgb(0xf4, 0xf1, 0xfa))
        .bg(Color::Rgb(0x30, 0x0f, 0x4a));
    assert_eq!(stack_header_style, expected_stack_header_style);
}

#[test]
fn too_small_overlay_is_bold_with_no_colors() {
    let too_small_overlay_style = compute_too_small_overlay_style();
    let expected_too_small_overlay_style = Style::default().add_modifier(Modifier::BOLD);
    assert_eq!(too_small_overlay_style, expected_too_small_overlay_style);
}

#[test]
fn letterbox_sets_only_the_backdrop() {
    let letterbox_style = compute_letterbox_style(&build_default_theme());
    let expected_letterbox_style = Style::default().bg(Color::Rgb(0x58, 0x58, 0x58));
    assert_eq!(letterbox_style, expected_letterbox_style);
}

#[test]
fn focused_border_is_the_focus_color_in_bold() {
    let focused_border_style = compute_focused_border_style(&build_default_theme());
    let expected_focused_border_style = Style::default()
        .fg(Color::Rgb(0x00, 0xaf, 0xd7))
        .add_modifier(Modifier::BOLD);
    assert_eq!(focused_border_style, expected_focused_border_style);
}

#[test]
fn unfocused_border_is_the_dim_color_without_bold() {
    let unfocused_border_style = compute_unfocused_border_style(&build_default_theme());
    let expected_unfocused_border_style = Style::default().fg(Color::Rgb(0x58, 0x58, 0x58));
    assert_eq!(unfocused_border_style, expected_unfocused_border_style);
}

#[test]
fn hover_border_is_the_hover_color_in_bold() {
    let hovered_border_style = compute_hover_border_style(&build_default_theme());
    let expected_hovered_border_style = Style::default()
        .fg(Color::Rgb(0xaf, 0x5f, 0xff))
        .add_modifier(Modifier::BOLD);
    assert_eq!(hovered_border_style, expected_hovered_border_style);
}
