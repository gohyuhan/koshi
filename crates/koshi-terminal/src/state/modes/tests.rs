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

#[test]
fn terminal_modes_default_serializes_to_the_resume_body_shape() {
    let json = serde_json::to_string(&TerminalModes::default()).expect("serializes");
    assert_eq!(
        json,
        r#"{"bracketed_paste":false,"mouse_tracking":"Off","mouse_encoding":"Default","alt_scroll":false,"autowrap":true,"app_cursor_keys":false,"reverse_video":false,"cursor_blink":false,"cursor_shape":null}"#
    );
}

#[test]
fn terminal_modes_with_every_value_flipped_round_trip_through_json() {
    let modes = TerminalModes {
        bracketed_paste: true,
        mouse_tracking: MouseTracking::AnyMotion,
        mouse_encoding: MouseEncoding::Sgr,
        alt_scroll: true,
        autowrap: false,
        app_cursor_keys: true,
        reverse_video: true,
        cursor_blink: true,
        cursor_shape: Some(CursorShape::Bar),
    };
    let json = serde_json::to_string(&modes).expect("serializes");
    assert_eq!(
        json,
        r#"{"bracketed_paste":true,"mouse_tracking":"AnyMotion","mouse_encoding":"Sgr","alt_scroll":true,"autowrap":false,"app_cursor_keys":true,"reverse_video":true,"cursor_blink":true,"cursor_shape":"Bar"}"#
    );
    let read_back: TerminalModes = serde_json::from_str(&json).expect("reads back");
    assert_eq!(read_back, modes);
}

#[test]
fn a_terminal_modes_body_without_cursor_shape_reads_back_as_none() {
    let body = r#"{"bracketed_paste":false,"mouse_tracking":"Off","mouse_encoding":"Default","alt_scroll":false,"autowrap":true,"app_cursor_keys":false,"reverse_video":false,"cursor_blink":false}"#;
    let read_back: TerminalModes = serde_json::from_str(body).expect("reads back");
    assert_eq!(read_back, TerminalModes::default());
}

#[test]
fn a_terminal_modes_body_missing_a_flag_is_rejected() {
    let body = r#"{"bracketed_paste":false,"mouse_tracking":"Off","mouse_encoding":"Default","alt_scroll":false,"autowrap":true,"app_cursor_keys":false,"reverse_video":false,"cursor_shape":null}"#;
    let error = serde_json::from_str::<TerminalModes>(body).expect_err("cursor_blink is required");
    assert_eq!(
        error.to_string(),
        format!(
            "missing field `cursor_blink` at line 1 column {}",
            body.len()
        )
    );
}

#[test]
fn cursor_shape_serializes_as_its_variant_name() {
    let shapes = [
        (CursorShape::Block, r#""Block""#),
        (CursorShape::Underline, r#""Underline""#),
        (CursorShape::Bar, r#""Bar""#),
    ];
    for (shape, json) in shapes {
        assert_eq!(serde_json::to_string(&shape).expect("serializes"), json);
        let read_back: CursorShape = serde_json::from_str(json).expect("reads back");
        assert_eq!(read_back, shape);
    }
}

#[test]
fn an_unknown_cursor_shape_name_is_rejected() {
    let error = serde_json::from_str::<CursorShape>(r#""Circle""#).expect_err("no such shape");
    assert_eq!(
        error.to_string(),
        "unknown variant `Circle`, expected one of `Block`, `Underline`, `Bar` at line 1 column 8"
    );
}

#[test]
fn mouse_encoding_serializes_as_its_variant_name() {
    let encodings = [
        (MouseEncoding::Default, r#""Default""#),
        (MouseEncoding::Utf8, r#""Utf8""#),
        (MouseEncoding::Sgr, r#""Sgr""#),
        (MouseEncoding::Urxvt, r#""Urxvt""#),
    ];
    for (encoding, json) in encodings {
        assert_eq!(serde_json::to_string(&encoding).expect("serializes"), json);
        let read_back: MouseEncoding = serde_json::from_str(json).expect("reads back");
        assert_eq!(read_back, encoding);
    }
}
