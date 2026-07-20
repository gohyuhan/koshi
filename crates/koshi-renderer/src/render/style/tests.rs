//! Tests for the chrome style helpers: each one turns the default theme into an
//! exact ratatui `Style`, and the tab styles invert between the active and
//! inactive tab.

use super::*;

/// The stock theme every case below reads its colors from.
fn theme() -> Theme {
    Theme::default()
}

#[test]
fn active_tab_index_is_its_ramp_stop_as_bold_text() {
    // Active: the ramp stop is the TEXT color, no block background.
    let got = tab_index_style(&theme(), true, 0, 1);
    let want = Style::default()
        .fg(Color::Rgb(0x58, 0x1c, 0x87))
        .add_modifier(Modifier::BOLD);
    assert_eq!(got, want);
}

#[test]
fn inactive_tab_index_is_quiet_text_on_the_dimmed_stop() {
    // Inactive: quiet text over the dimmed ramp stop as the block background.
    let got = tab_index_style(&theme(), false, 0, 1);
    let want = Style::default()
        .fg(Color::Rgb(0xc9, 0xc4, 0xd4))
        .bg(Color::Rgb(0x30, 0x0f, 0x4a));
    assert_eq!(got, want);
}

#[test]
fn active_tab_name_is_its_ramp_stop_without_bold() {
    // The name block takes the same inversion as the `#N` block but is not bold.
    let got = tab_name_style(&theme(), true, 0, 1);
    let want = Style::default().fg(Color::Rgb(0x58, 0x1c, 0x87));
    assert_eq!(got, want);
}

#[test]
fn inactive_tab_name_matches_the_inactive_index_block() {
    let got = tab_name_style(&theme(), false, 0, 1);
    let want = Style::default()
        .fg(Color::Rgb(0xc9, 0xc4, 0xd4))
        .bg(Color::Rgb(0x30, 0x0f, 0x4a));
    assert_eq!(got, want);
}

#[test]
fn a_middle_tab_stop_reads_the_blended_ramp_color() {
    // The style helper passes index/count straight to `Theme::ramp`, so tab 1
    // of 3 gets the blended middle stop, not an endpoint.
    let got = tab_index_style(&theme(), true, 1, 3);
    let want = Style::default()
        .fg(Color::Rgb(0x4a, 0x4f, 0xbe))
        .add_modifier(Modifier::BOLD);
    assert_eq!(got, want);
}

#[test]
fn session_block_is_the_ramp_start_end_as_bold_text() {
    let got = session_style(&theme());
    let want = Style::default()
        .fg(Color::Rgb(0x58, 0x1c, 0x87))
        .add_modifier(Modifier::BOLD);
    assert_eq!(got, want);
}

#[test]
fn mode_tag_is_the_ramp_far_end_as_bold_text() {
    let got = mode_style(&theme());
    let want = Style::default()
        .fg(Color::Rgb(0x3b, 0x82, 0xf6))
        .add_modifier(Modifier::BOLD);
    assert_eq!(got, want);
}

#[test]
fn scroll_arrow_is_quiet_text_in_bold() {
    let got = scroll_arrow_style(&theme());
    let want = Style::default()
        .fg(Color::Rgb(0xc9, 0xc4, 0xd4))
        .add_modifier(Modifier::BOLD);
    assert_eq!(got, want);
}

#[test]
fn stack_header_uses_its_own_two_colors() {
    let got = stack_header_style(&theme());
    let want = Style::default()
        .fg(Color::Rgb(0xf4, 0xf1, 0xfa))
        .bg(Color::Rgb(0x30, 0x0f, 0x4a));
    assert_eq!(got, want);
}

#[test]
fn too_small_overlay_is_bold_with_no_colors() {
    let got = too_small_style();
    let want = Style::default().add_modifier(Modifier::BOLD);
    assert_eq!(got, want);
}

#[test]
fn letterbox_sets_only_the_backdrop() {
    let got = letterbox_style(&theme());
    let want = Style::default().bg(Color::Rgb(0x58, 0x58, 0x58));
    assert_eq!(got, want);
}

#[test]
fn focused_border_is_the_focus_color_in_bold() {
    let got = border_focused_style(&theme());
    let want = Style::default()
        .fg(Color::Rgb(0x00, 0xaf, 0xd7))
        .add_modifier(Modifier::BOLD);
    assert_eq!(got, want);
}

#[test]
fn unfocused_border_is_the_dim_color_without_bold() {
    let got = border_unfocused_style(&theme());
    let want = Style::default().fg(Color::Rgb(0x58, 0x58, 0x58));
    assert_eq!(got, want);
}

#[test]
fn hover_border_is_the_hover_color_in_bold() {
    let got = border_hover_style(&theme());
    let want = Style::default()
        .fg(Color::Rgb(0xaf, 0x5f, 0xff))
        .add_modifier(Modifier::BOLD);
    assert_eq!(got, want);
}
