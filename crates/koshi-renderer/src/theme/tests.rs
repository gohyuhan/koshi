//! Tests for the chrome ramp: endpoint stops, monotonic blend, the
//! single-element run, and the dimmed variant.

use super::*;

#[test]
fn ramp_endpoints_are_the_palette_ends() {
    assert_eq!(ramp(0, 5), Color::Rgb(0x58, 0x1c, 0x87));
    assert_eq!(ramp(4, 5), Color::Rgb(0x3b, 0x82, 0xf6));
}

#[test]
fn a_single_element_run_takes_the_purple_end() {
    assert_eq!(ramp(0, 1), Color::Rgb(0x58, 0x1c, 0x87));
}

#[test]
fn an_out_of_range_index_clamps_to_the_blue_end() {
    assert_eq!(ramp(9, 3), ramp(2, 3));
}

#[test]
fn middle_stops_sit_between_the_ends() {
    let Color::Rgb(r, g, b) = ramp(1, 3) else {
        panic!("ramp yields Rgb");
    };
    assert_eq!((r, g, b), (0x4a, 0x4f, 0xbe));
}

#[test]
fn the_dim_variant_darkens_every_channel() {
    let Color::Rgb(r, g, b) = ramp_dim(0, 1) else {
        panic!("ramp_dim yields Rgb");
    };
    assert_eq!((r, g, b), (0x30, 0x0f, 0x4a));
}
