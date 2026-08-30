//! Tests for the chrome theme: default ramp endpoint stops, monotonic blend,
//! the single-element run, the dimmed variant, a custom-endpoint ramp, and the
//! stock palette the default theme carries.

use super::*;

use koshi_config::types::{ColorPalette, RgbColor};

#[test]
fn ramp_endpoints_are_the_palette_ends() {
    let theme = Theme::default();
    assert_eq!(theme.ramp(0, 5), Color::Rgb(0xd0, 0xa5, 0xff));
    assert_eq!(theme.ramp(4, 5), Color::Rgb(0x7d, 0xbc, 0xff));
}

#[test]
fn a_single_element_run_takes_the_start_end() {
    assert_eq!(Theme::default().ramp(0, 1), Color::Rgb(0xd0, 0xa5, 0xff));
}

#[test]
fn an_out_of_range_index_clamps_to_the_last_end() {
    let theme = Theme::default();
    assert_eq!(theme.ramp(9, 3), Color::Rgb(0x7d, 0xbc, 0xff));
    assert_eq!(theme.ramp(2, 3), Color::Rgb(0x7d, 0xbc, 0xff));
}

#[test]
fn middle_stops_sit_between_the_ends() {
    let Color::Rgb(r, g, b) = Theme::default().ramp(1, 3) else {
        panic!("ramp yields Rgb");
    };
    assert_eq!((r, g, b), (0xa7, 0xb0, 0xff));
}

#[test]
fn the_dim_variant_darkens_every_channel() {
    let Color::Rgb(r, g, b) = Theme::default().ramp_dim(0, 1) else {
        panic!("ramp_dim yields Rgb");
    };
    assert_eq!((r, g, b), (0x72, 0x5a, 0x8c));
}

#[test]
fn a_zero_count_run_returns_the_start_end_without_dividing_by_zero() {
    // `count == 0` drives `den == 0` inside `lerp`; the explicit guard there
    // must return the start channel rather than dividing by zero.
    let theme = Theme::default();
    assert_eq!(theme.ramp(0, 0), Color::Rgb(0xd0, 0xa5, 0xff));
    assert_eq!(theme.ramp(7, 0), Color::Rgb(0xd0, 0xa5, 0xff));
}

#[test]
fn the_dim_variant_tracks_the_ramp_stop_it_darkens() {
    // The dim of the far ramp end is that end pulled to 55% of each channel.
    let theme = Theme::default();
    assert_eq!(theme.ramp_dim(1, 2), Color::Rgb(0x44, 0x67, 0x8c));
}

#[test]
fn every_stop_of_a_five_element_run_is_exact() {
    let theme = Theme::default();
    let stops: Vec<Color> = (0..5).map(|index| theme.ramp(index, 5)).collect();
    assert_eq!(
        stops,
        vec![
            Color::Rgb(0xd0, 0xa5, 0xff),
            Color::Rgb(0xbc, 0xaa, 0xff),
            Color::Rgb(0xa7, 0xb0, 0xff),
            Color::Rgb(0x92, 0xb6, 0xff),
            Color::Rgb(0x7d, 0xbc, 0xff),
        ]
    );
}

#[test]
fn an_index_at_and_past_the_run_length_clamps_to_the_last_stop() {
    let theme = Theme::default();
    assert_eq!(theme.ramp(3, 3), Color::Rgb(0x7d, 0xbc, 0xff));
    assert_eq!(theme.ramp(usize::MAX, 3), Color::Rgb(0x7d, 0xbc, 0xff));
}

#[test]
fn the_dim_variant_clamps_its_index_and_count_the_way_the_ramp_does() {
    let theme = Theme::default();
    // Past the last stop of a three-element run, and a zero-element run.
    assert_eq!(theme.ramp_dim(9, 3), Color::Rgb(0x44, 0x67, 0x8c));
    assert_eq!(theme.ramp_dim(0, 0), Color::Rgb(0x72, 0x5a, 0x8c));
}

#[test]
fn the_dim_variant_leaves_black_black_and_pulls_white_to_fifty_five_percent() {
    let theme = Theme {
        ramp_start: (0x00, 0x00, 0x00),
        ramp_end: (0xff, 0xff, 0xff),
        ..Theme::default()
    };
    assert_eq!(theme.ramp_dim(0, 2), Color::Rgb(0x00, 0x00, 0x00));
    assert_eq!(theme.ramp_dim(1, 2), Color::Rgb(0x8c, 0x8c, 0x8c));
}

#[test]
fn the_default_theme_is_the_config_crates_default_palette() {
    let theme = Theme::default();
    let palette = ColorPalette::default();
    let rgb = |color: RgbColor| Color::Rgb(color.r, color.g, color.b);
    let channels = |color: RgbColor| (color.r, color.g, color.b);

    assert_eq!(theme.ramp_start, channels(palette.ramp_start));
    assert_eq!(theme.ramp_end, channels(palette.ramp_end));
    assert_eq!(theme.on_ramp, rgb(palette.on_ramp));
    assert_eq!(theme.on_ramp_dim, rgb(palette.on_ramp_dim));
    assert_eq!(theme.accent, rgb(palette.accent));
    assert_eq!(theme.on_accent, rgb(palette.on_accent));
    assert_eq!(theme.border_focused, rgb(palette.border_focused));
    assert_eq!(theme.border_unfocused, rgb(palette.border_unfocused));
    assert_eq!(theme.border_hover, rgb(palette.border_hover));
    assert_eq!(theme.stack_header_fg, rgb(palette.stack_header_fg));
    assert_eq!(theme.stack_header_bg, rgb(palette.stack_header_bg));
    assert_eq!(theme.letterbox, rgb(palette.letterbox));
    assert_eq!(theme.bar_bg, rgb(palette.bar_bg));
}

#[test]
fn every_default_color_outside_the_ramp_is_exact() {
    let theme = Theme::default();
    assert_eq!(theme.on_ramp, Color::Rgb(0x12, 0x09, 0x1f));
    assert_eq!(theme.on_ramp_dim, Color::Rgb(0xf0, 0xec, 0xfa));
    assert_eq!(theme.accent, Color::Rgb(0xf5, 0xc2, 0xff));
    assert_eq!(theme.on_accent, Color::Rgb(0x1e, 0x10, 0x33));
    assert_eq!(theme.border_focused, Color::Rgb(0x00, 0xaf, 0xd7));
    assert_eq!(theme.border_unfocused, Color::Rgb(0x58, 0x58, 0x58));
    assert_eq!(theme.border_hover, Color::Rgb(0xaf, 0x5f, 0xff));
    assert_eq!(theme.stack_header_fg, Color::Rgb(0xf4, 0xf1, 0xfa));
    assert_eq!(theme.stack_header_bg, Color::Rgb(0x30, 0x0f, 0x4a));
    assert_eq!(theme.letterbox, Color::Rgb(0x58, 0x58, 0x58));
    assert_eq!(theme.bar_bg, Color::Rgb(0x00, 0x00, 0x00));
}

#[test]
fn custom_endpoints_drive_the_ramp() {
    let theme = Theme {
        ramp_start: (0xff, 0x00, 0x00),
        ramp_end: (0x00, 0x00, 0xff),
        ..Theme::default()
    };
    assert_eq!(theme.ramp(0, 2), Color::Rgb(0xff, 0x00, 0x00));
    assert_eq!(theme.ramp(1, 2), Color::Rgb(0x00, 0x00, 0xff));
    // Midpoint by integer lerp: red truncates toward zero (255 - 255/2 = 128).
    assert_eq!(theme.ramp(1, 3), Color::Rgb(0x80, 0x00, 0x7f));
}
