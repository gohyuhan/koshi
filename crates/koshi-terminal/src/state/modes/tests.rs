//! Unit tests for the terminal mode flags and their default startup values.

use super::*;

#[test]
fn terminal_modes_default_matches_the_documented_startup_state() {
    let modes = TerminalModes::default();
    assert!(!modes.bracketed_paste);
    assert_eq!(modes.mouse_tracking, MouseTracking::Off);
    assert_eq!(modes.mouse_encoding, MouseEncoding::Default);
    assert!(!modes.alt_scroll);
    // Autowrap (DECAWM `?7`) starts on; every other bool flag starts off.
    assert!(modes.autowrap);
    assert!(!modes.app_cursor_keys);
    assert!(!modes.reverse_video);
    assert!(!modes.cursor_blink);
    assert_eq!(modes.cursor_shape, None);
}

#[test]
fn mouse_tracking_default_is_off() {
    assert_eq!(MouseTracking::default(), MouseTracking::Off);
}

#[test]
fn mouse_encoding_default_is_the_legacy_single_byte_form() {
    assert_eq!(MouseEncoding::default(), MouseEncoding::Default);
}

#[test]
fn the_four_mouse_tracking_levels_are_distinct() {
    let levels = [
        MouseTracking::Off,
        MouseTracking::X10,
        MouseTracking::Normal,
        MouseTracking::ButtonMotion,
        MouseTracking::AnyMotion,
    ];
    for (i, a) in levels.iter().enumerate() {
        for (j, b) in levels.iter().enumerate() {
            assert_eq!(a == b, i == j);
        }
    }
}

#[test]
fn the_four_mouse_encodings_are_distinct() {
    let encodings = [
        MouseEncoding::Default,
        MouseEncoding::Utf8,
        MouseEncoding::Sgr,
        MouseEncoding::Urxvt,
    ];
    for (i, a) in encodings.iter().enumerate() {
        for (j, b) in encodings.iter().enumerate() {
            assert_eq!(a == b, i == j);
        }
    }
}

#[test]
fn the_three_cursor_shapes_are_distinct() {
    let shapes = [CursorShape::Block, CursorShape::Underline, CursorShape::Bar];
    for (i, a) in shapes.iter().enumerate() {
        for (j, b) in shapes.iter().enumerate() {
            assert_eq!(a == b, i == j);
        }
    }
}
