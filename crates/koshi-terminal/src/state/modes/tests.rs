//! Unit tests for the terminal mode flags and their default startup values.

use super::*;

#[test]
fn terminal_modes_default_matches_the_documented_startup_state() {
    let modes = TerminalModes::default();
    assert!(!modes.is_bracketed_paste_enabled);
    assert_eq!(modes.mouse_tracking, MouseTracking::Off);
    assert_eq!(modes.mouse_encoding, MouseEncoding::Default);
    assert!(!modes.is_alternate_scroll_enabled);
    // Autowrap (DECAWM `?7`), Sixel scrolling, and private Sixel registers
    // start on; the other mode flags start off.
    assert!(modes.is_autowrap_enabled);
    assert!(modes.is_sixel_scrolling_enabled);
    assert!(modes.is_sixel_private_color_registers_enabled);
    assert!(!modes.is_sixel_cursor_right_enabled);
    assert!(!modes.is_application_cursor_keys_enabled);
    assert!(!modes.is_reverse_video_enabled);
    assert!(!modes.is_cursor_blink_enabled);
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
    for (first_level_index, first_level) in levels.iter().enumerate() {
        for (second_level_index, second_level) in levels.iter().enumerate() {
            assert_eq!(
                first_level == second_level,
                first_level_index == second_level_index
            );
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
    for (first_encoding_index, first_encoding) in encodings.iter().enumerate() {
        for (second_encoding_index, second_encoding) in encodings.iter().enumerate() {
            assert_eq!(
                first_encoding == second_encoding,
                first_encoding_index == second_encoding_index
            );
        }
    }
}

#[test]
fn the_three_cursor_shapes_are_distinct() {
    let shapes = [CursorShape::Block, CursorShape::Underline, CursorShape::Bar];
    for (first_shape_index, first_shape) in shapes.iter().enumerate() {
        for (second_shape_index, second_shape) in shapes.iter().enumerate() {
            assert_eq!(
                first_shape == second_shape,
                first_shape_index == second_shape_index
            );
        }
    }
}

#[test]
fn terminal_modes_default_serializes_to_the_resume_body_shape() {
    let json = serde_json::to_string(&TerminalModes::default()).expect("serializes");
    assert_eq!(
        json,
        r#"{"is_bracketed_paste_enabled":false,"mouse_tracking":"Off","mouse_encoding":"Default","is_alternate_scroll_enabled":false,"is_autowrap_enabled":true,"is_application_cursor_keys_enabled":false,"is_left_right_margin_mode_enabled":false,"is_reverse_video_enabled":false,"is_cursor_blink_enabled":false,"cursor_shape":null,"is_sixel_scrolling_enabled":true,"is_sixel_private_color_registers_enabled":true,"is_sixel_cursor_right_enabled":false}"#
    );
}

#[test]
fn terminal_modes_with_every_value_flipped_round_trip_through_json() {
    let modes = TerminalModes {
        is_bracketed_paste_enabled: true,
        mouse_tracking: MouseTracking::AnyMotion,
        mouse_encoding: MouseEncoding::Sgr,
        is_alternate_scroll_enabled: true,
        is_autowrap_enabled: false,
        is_application_cursor_keys_enabled: true,
        is_left_right_margin_mode_enabled: true,
        is_reverse_video_enabled: true,
        is_cursor_blink_enabled: true,
        cursor_shape: Some(CursorShape::Bar),
        is_sixel_scrolling_enabled: false,
        is_sixel_private_color_registers_enabled: false,
        is_sixel_cursor_right_enabled: true,
    };
    let json = serde_json::to_string(&modes).expect("serializes");
    assert_eq!(
        json,
        r#"{"is_bracketed_paste_enabled":true,"mouse_tracking":"AnyMotion","mouse_encoding":"Sgr","is_alternate_scroll_enabled":true,"is_autowrap_enabled":false,"is_application_cursor_keys_enabled":true,"is_left_right_margin_mode_enabled":true,"is_reverse_video_enabled":true,"is_cursor_blink_enabled":true,"cursor_shape":"Bar","is_sixel_scrolling_enabled":false,"is_sixel_private_color_registers_enabled":false,"is_sixel_cursor_right_enabled":true}"#
    );
    let read_back: TerminalModes = serde_json::from_str(&json).expect("reads back");
    assert_eq!(read_back, modes);
}

#[test]
fn a_terminal_modes_body_without_cursor_shape_reads_back_as_none() {
    let serialized_modes_json = r#"{"is_bracketed_paste_enabled":false,"mouse_tracking":"Off","mouse_encoding":"Default","is_alternate_scroll_enabled":false,"is_autowrap_enabled":true,"is_application_cursor_keys_enabled":false,"is_left_right_margin_mode_enabled":false,"is_reverse_video_enabled":false,"is_cursor_blink_enabled":false,"is_sixel_scrolling_enabled":true,"is_sixel_private_color_registers_enabled":true,"is_sixel_cursor_right_enabled":false}"#;
    let read_back: TerminalModes = serde_json::from_str(serialized_modes_json).expect("reads back");
    assert_eq!(read_back, TerminalModes::default());
}

#[test]
fn a_terminal_modes_body_missing_a_flag_is_rejected() {
    let serialized_modes_json = r#"{"is_bracketed_paste_enabled":false,"mouse_tracking":"Off","mouse_encoding":"Default","is_alternate_scroll_enabled":false,"is_autowrap_enabled":true,"is_application_cursor_keys_enabled":false,"is_left_right_margin_mode_enabled":false,"is_reverse_video_enabled":false,"cursor_shape":null,"is_sixel_scrolling_enabled":true,"is_sixel_private_color_registers_enabled":true,"is_sixel_cursor_right_enabled":false}"#;
    let deserialize_error = serde_json::from_str::<TerminalModes>(serialized_modes_json)
        .expect_err("is_cursor_blink_enabled is required");
    assert_eq!(
        deserialize_error.to_string(),
        format!(
            "missing field `is_cursor_blink_enabled` at line 1 column {}",
            serialized_modes_json.len()
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
    let deserialize_error =
        serde_json::from_str::<CursorShape>(r#""Circle""#).expect_err("no such shape");
    assert_eq!(
        deserialize_error.to_string(),
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
