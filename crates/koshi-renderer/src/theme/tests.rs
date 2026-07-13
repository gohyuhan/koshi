//! Tests for the chrome theme: default ramp endpoint stops, monotonic blend,
//! the single-element run, the dimmed variant, and a custom-endpoint ramp.

use super::*;

#[test]
fn ramp_endpoints_are_the_palette_ends() {
    let theme = Theme::default();
    assert_eq!(theme.ramp(0, 5), Color::Rgb(0x58, 0x1c, 0x87));
    assert_eq!(theme.ramp(4, 5), Color::Rgb(0x3b, 0x82, 0xf6));
}

#[test]
fn a_single_element_run_takes_the_start_end() {
    assert_eq!(Theme::default().ramp(0, 1), Color::Rgb(0x58, 0x1c, 0x87));
}

#[test]
fn an_out_of_range_index_clamps_to_the_last_end() {
    let theme = Theme::default();
    assert_eq!(theme.ramp(9, 3), theme.ramp(2, 3));
}

#[test]
fn middle_stops_sit_between_the_ends() {
    let Color::Rgb(r, g, b) = Theme::default().ramp(1, 3) else {
        panic!("ramp yields Rgb");
    };
    assert_eq!((r, g, b), (0x4a, 0x4f, 0xbe));
}

#[test]
fn the_dim_variant_darkens_every_channel() {
    let Color::Rgb(r, g, b) = Theme::default().ramp_dim(0, 1) else {
        panic!("ramp_dim yields Rgb");
    };
    assert_eq!((r, g, b), (0x30, 0x0f, 0x4a));
}

#[test]
fn a_zero_count_run_returns_the_start_end_without_dividing_by_zero() {
    // `count == 0` drives `den == 0` inside `lerp`; the explicit guard there
    // must return the start channel rather than dividing by zero.
    let theme = Theme::default();
    assert_eq!(theme.ramp(0, 0), Color::Rgb(0x58, 0x1c, 0x87));
    assert_eq!(theme.ramp(7, 0), Color::Rgb(0x58, 0x1c, 0x87));
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
