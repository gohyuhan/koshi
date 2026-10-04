//! Mouse-report encoding tests: exact bytes per encoding, the modifier and
//! motion bits, and the tracking-level ladder that decides what is reported.

use super::*;

const ANY_MOTION_TRACKING: MouseTracking = MouseTracking::AnyMotion;

// --- SGR encoding: `CSI < button_code ; column ; row M/m` ------------------

#[test]
fn sgr_left_press_ends_in_upper_m_and_its_release_in_lower_m() {
    assert_eq!(
        encode_mouse(
            MouseKind::Press(MouseButton::Left),
            BindingModifierFlags::NONE,
            1,
            1,
            ANY_MOTION_TRACKING,
            MouseEncoding::Sgr
        ),
        Some(b"\x1b[<0;1;1M".to_vec())
    );
    assert_eq!(
        encode_mouse(
            MouseKind::Release(MouseButton::Left),
            BindingModifierFlags::NONE,
            1,
            1,
            ANY_MOTION_TRACKING,
            MouseEncoding::Sgr
        ),
        Some(b"\x1b[<0;1;1m".to_vec()),
        "a release keeps the button and ends with a lowercase m"
    );
}

#[test]
fn sgr_press_reports_left_0_middle_1_right_2() {
    let encode_mouse_button = |mouse_button| {
        encode_mouse(
            MouseKind::Press(mouse_button),
            BindingModifierFlags::NONE,
            1,
            1,
            ANY_MOTION_TRACKING,
            MouseEncoding::Sgr,
        )
        .unwrap()
    };
    assert_eq!(encode_mouse_button(MouseButton::Left), b"\x1b[<0;1;1M");
    assert_eq!(encode_mouse_button(MouseButton::Middle), b"\x1b[<1;1;1M");
    assert_eq!(encode_mouse_button(MouseButton::Right), b"\x1b[<2;1;1M");
}

#[test]
fn sgr_modifier_bits_add_shift_alt_ctrl() {
    let encode_mouse_with_modifiers = |modifier_flags| {
        encode_mouse(
            MouseKind::Press(MouseButton::Left),
            modifier_flags,
            1,
            1,
            ANY_MOTION_TRACKING,
            MouseEncoding::Sgr,
        )
        .unwrap()
    };
    assert_eq!(
        encode_mouse_with_modifiers(BindingModifierFlags::SHIFT),
        b"\x1b[<4;1;1M"
    );
    assert_eq!(
        encode_mouse_with_modifiers(BindingModifierFlags::ALT),
        b"\x1b[<8;1;1M"
    );
    assert_eq!(
        encode_mouse_with_modifiers(BindingModifierFlags::CTRL),
        b"\x1b[<16;1;1M"
    );
    assert_eq!(
        encode_mouse_with_modifiers(BindingModifierFlags::CTRL.union(BindingModifierFlags::SHIFT)),
        b"\x1b[<20;1;1M",
        "modifier bits sum"
    );
    assert_eq!(
        encode_mouse_with_modifiers(BindingModifierFlags::SUPER),
        b"\x1b[<0;1;1M",
        "super has no protocol bit"
    );
}

#[test]
fn sgr_drag_and_motion_set_the_motion_bit() {
    assert_eq!(
        encode_mouse(
            MouseKind::Drag(MouseButton::Left),
            BindingModifierFlags::NONE,
            2,
            3,
            ANY_MOTION_TRACKING,
            MouseEncoding::Sgr
        ),
        Some(b"\x1b[<32;2;3M".to_vec()),
        "drag = button 0 + motion bit 32"
    );
    assert_eq!(
        encode_mouse(
            MouseKind::Motion,
            BindingModifierFlags::NONE,
            5,
            6,
            ANY_MOTION_TRACKING,
            MouseEncoding::Sgr
        ),
        Some(b"\x1b[<35;5;6M".to_vec()),
        "bare motion = no-button 3 + motion bit 32"
    );
}

#[test]
fn sgr_wheel_reports_up_64_down_65_left_66_right_67() {
    let encode_scroll_direction = |scroll_direction| {
        encode_mouse(
            MouseKind::Scroll(scroll_direction),
            BindingModifierFlags::NONE,
            1,
            1,
            ANY_MOTION_TRACKING,
            MouseEncoding::Sgr,
        )
        .unwrap()
    };
    assert_eq!(
        encode_scroll_direction(ScrollDirection::Up),
        b"\x1b[<64;1;1M"
    );
    assert_eq!(
        encode_scroll_direction(ScrollDirection::Down),
        b"\x1b[<65;1;1M"
    );
    assert_eq!(
        encode_scroll_direction(ScrollDirection::Left),
        b"\x1b[<66;1;1M"
    );
    assert_eq!(
        encode_scroll_direction(ScrollDirection::Right),
        b"\x1b[<67;1;1M"
    );
}

#[test]
fn sgr_release_keeps_its_button_and_its_modifiers() {
    assert_eq!(
        encode_mouse(
            MouseKind::Release(MouseButton::Right),
            BindingModifierFlags::ALT,
            1,
            1,
            ANY_MOTION_TRACKING,
            MouseEncoding::Sgr,
        ),
        Some(b"\x1b[<10;1;1m".to_vec()),
        "right 2 + alt 8, lowercase m"
    );
    assert_eq!(
        encode_mouse(
            MouseKind::Release(MouseButton::Middle),
            BindingModifierFlags::NONE,
            1,
            1,
            ANY_MOTION_TRACKING,
            MouseEncoding::Sgr,
        ),
        Some(b"\x1b[<1;1;1m".to_vec())
    );
}

#[test]
fn sgr_all_three_modifiers_sum_to_twenty_eight() {
    assert_eq!(
        encode_mouse(
            MouseKind::Press(MouseButton::Left),
            BindingModifierFlags::SHIFT
                .union(BindingModifierFlags::ALT)
                .union(BindingModifierFlags::CTRL),
            1,
            1,
            ANY_MOTION_TRACKING,
            MouseEncoding::Sgr,
        ),
        Some(b"\x1b[<28;1;1M".to_vec())
    );
}

#[test]
fn sgr_writes_large_cells_in_decimal_without_a_cap() {
    assert_eq!(
        encode_mouse(
            MouseKind::Press(MouseButton::Left),
            BindingModifierFlags::NONE,
            300,
            400,
            ANY_MOTION_TRACKING,
            MouseEncoding::Sgr,
        ),
        Some(b"\x1b[<0;300;400M".to_vec())
    );
    assert_eq!(
        encode_mouse(
            MouseKind::Press(MouseButton::Left),
            BindingModifierFlags::NONE,
            u16::MAX,
            u16::MAX,
            ANY_MOTION_TRACKING,
            MouseEncoding::Sgr,
        ),
        Some(b"\x1b[<0;65535;65535M".to_vec())
    );
}

// --- Legacy / UTF-8 / urxvt encodings --------------------------------------

#[test]
fn legacy_press_reports_its_button_and_its_release_reports_3() {
    assert_eq!(
        encode_mouse(
            MouseKind::Press(MouseButton::Left),
            BindingModifierFlags::NONE,
            1,
            1,
            ANY_MOTION_TRACKING,
            MouseEncoding::Default
        ),
        Some(vec![0x1b, b'[', b'M', 32, 33, 33]),
        "button code 0, column 1, row 1, each offset by 32"
    );
    assert_eq!(
        encode_mouse(
            MouseKind::Release(MouseButton::Left),
            BindingModifierFlags::NONE,
            1,
            1,
            ANY_MOTION_TRACKING,
            MouseEncoding::Default
        ),
        Some(vec![0x1b, b'[', b'M', 35, 33, 33]),
        "a legacy release loses the button and reports 3 (offset to 35)"
    );
}

#[test]
fn legacy_caps_a_cell_past_the_byte_limit() {
    let mouse_report_bytes = encode_mouse(
        MouseKind::Press(MouseButton::Left),
        BindingModifierFlags::NONE,
        300,
        1,
        ANY_MOTION_TRACKING,
        MouseEncoding::Default,
    )
    .unwrap();
    assert_eq!(
        mouse_report_bytes,
        vec![0x1b, b'[', b'M', 32, 255, 33],
        "column 300 + 32 does not fit a byte: the byte saturates at 255"
    );
}

#[test]
fn legacy_saturates_at_the_last_cell_a_byte_holds_and_one_past_it() {
    let encode_mouse_at_column_index = |column_index| {
        encode_mouse(
            MouseKind::Press(MouseButton::Left),
            BindingModifierFlags::NONE,
            column_index,
            1,
            ANY_MOTION_TRACKING,
            MouseEncoding::Default,
        )
        .unwrap()
    };
    // Column 223 + 32 is exactly 255; column 224 + 32 is 256 and saturates at
    // 255.
    assert_eq!(
        encode_mouse_at_column_index(223),
        vec![0x1b, b'[', b'M', 32, 255, 33]
    );
    assert_eq!(
        encode_mouse_at_column_index(224),
        vec![0x1b, b'[', b'M', 32, 255, 33]
    );
}

#[test]
fn legacy_caps_the_row_byte_the_same_way_as_the_column() {
    assert_eq!(
        encode_mouse(
            MouseKind::Press(MouseButton::Left),
            BindingModifierFlags::NONE,
            1,
            300,
            ANY_MOTION_TRACKING,
            MouseEncoding::Default,
        ),
        Some(vec![0x1b, b'[', b'M', 32, 33, 255])
    );
}

#[test]
fn legacy_writes_a_zero_cell_as_bare_offsets() {
    assert_eq!(
        encode_mouse(
            MouseKind::Press(MouseButton::Left),
            BindingModifierFlags::NONE,
            0,
            0,
            ANY_MOTION_TRACKING,
            MouseEncoding::Default,
        ),
        Some(vec![0x1b, b'[', b'M', 32, 32, 32])
    );
}

#[test]
fn legacy_release_of_every_button_reports_three() {
    let encode_mouse_release = |mouse_button| {
        encode_mouse(
            MouseKind::Release(mouse_button),
            BindingModifierFlags::NONE,
            1,
            1,
            ANY_MOTION_TRACKING,
            MouseEncoding::Default,
        )
        .unwrap()
    };
    assert_eq!(
        encode_mouse_release(MouseButton::Middle),
        vec![0x1b, b'[', b'M', 35, 33, 33]
    );
    assert_eq!(
        encode_mouse_release(MouseButton::Right),
        vec![0x1b, b'[', b'M', 35, 33, 33]
    );
}

#[test]
fn legacy_drag_and_wheel_add_their_bits_to_the_modifiers() {
    // Middle drag with shift: button 1 + motion 32 + shift 4 = 37, offset to 69.
    assert_eq!(
        encode_mouse(
            MouseKind::Drag(MouseButton::Middle),
            BindingModifierFlags::SHIFT,
            1,
            1,
            ANY_MOTION_TRACKING,
            MouseEncoding::Default,
        ),
        Some(vec![0x1b, b'[', b'M', 69, 33, 33])
    );
    // Wheel down with ctrl: 65 + 16 = 81, offset to 113.
    assert_eq!(
        encode_mouse(
            MouseKind::Scroll(ScrollDirection::Down),
            BindingModifierFlags::CTRL,
            1,
            1,
            ANY_MOTION_TRACKING,
            MouseEncoding::Default,
        ),
        Some(vec![0x1b, b'[', b'M', 113, 33, 33])
    );
}

#[test]
fn a_coordinate_near_u16_max_saturates_without_overflowing() {
    // `column_number + 32` is summed in `u32`: the legacy byte saturates at 255
    // and the UTF-8 form writes a four-byte sequence.
    let legacy_mouse_report_bytes = encode_mouse(
        MouseKind::Press(MouseButton::Left),
        BindingModifierFlags::NONE,
        u16::MAX,
        1,
        MouseTracking::Normal,
        MouseEncoding::Default,
    )
    .unwrap();
    assert_eq!(
        legacy_mouse_report_bytes,
        vec![0x1b, b'[', b'M', 32, 255, 33],
        "the column byte saturates at 255"
    );

    // 65535 + 32 = 65567 = U+1001F, a four-byte UTF-8 sequence.
    assert_eq!(
        encode_mouse(
            MouseKind::Press(MouseButton::Left),
            BindingModifierFlags::NONE,
            u16::MAX,
            1,
            MouseTracking::Normal,
            MouseEncoding::Utf8,
        ),
        Some(vec![0x1b, b'[', b'M', 32, 0xf0, 0x90, 0x80, 0x9f, 33]),
        "the UTF-8 form encodes a huge coordinate without panicking"
    );
}

#[test]
fn utf8_switches_from_one_byte_to_two_at_cell_ninety_six() {
    let encode_mouse_at_column_index = |column_index| {
        encode_mouse(
            MouseKind::Press(MouseButton::Left),
            BindingModifierFlags::NONE,
            column_index,
            1,
            ANY_MOTION_TRACKING,
            MouseEncoding::Utf8,
        )
        .unwrap()
    };
    // 95 + 32 = 127 is the last one-byte code point; 96 + 32 = 128 is U+0080.
    assert_eq!(
        encode_mouse_at_column_index(95),
        vec![0x1b, b'[', b'M', 32, 0x7f, 33]
    );
    assert_eq!(
        encode_mouse_at_column_index(96),
        vec![0x1b, b'[', b'M', 32, 0xc2, 0x80, 33]
    );
}

#[test]
fn utf8_writes_a_cell_that_lands_on_a_surrogate_as_a_question_mark() {
    let encode_mouse_at_column_index = |column_index| {
        encode_mouse(
            MouseKind::Press(MouseButton::Left),
            BindingModifierFlags::NONE,
            column_index,
            1,
            ANY_MOTION_TRACKING,
            MouseEncoding::Utf8,
        )
        .unwrap()
    };
    // 55263 + 32 = U+D7FF, the last code point before the surrogates.
    assert_eq!(
        encode_mouse_at_column_index(55263),
        vec![0x1b, b'[', b'M', 32, 0xed, 0x9f, 0xbf, 33]
    );
    // 55264 + 32 = U+D800 and 57311 + 32 = U+DFFF are surrogates: not a `char`.
    assert_eq!(
        encode_mouse_at_column_index(55264),
        vec![0x1b, b'[', b'M', 32, b'?', 33]
    );
    assert_eq!(
        encode_mouse_at_column_index(57311),
        vec![0x1b, b'[', b'M', 32, b'?', 33]
    );
    // 57312 + 32 = U+E000, the first code point after them.
    assert_eq!(
        encode_mouse_at_column_index(57312),
        vec![0x1b, b'[', b'M', 32, 0xee, 0x80, 0x80, 33]
    );
}

#[test]
fn utf8_button_byte_with_every_bit_set_stays_one_byte() {
    // Wheel right 67 + shift 4 + alt 8 + ctrl 16 = 95, offset to 127: the
    // largest button code, still a single byte.
    assert_eq!(
        encode_mouse(
            MouseKind::Scroll(ScrollDirection::Right),
            BindingModifierFlags::SHIFT
                .union(BindingModifierFlags::ALT)
                .union(BindingModifierFlags::CTRL),
            1,
            1,
            ANY_MOTION_TRACKING,
            MouseEncoding::Utf8,
        ),
        Some(vec![0x1b, b'[', b'M', 0x7f, 33, 33])
    );
}

#[test]
fn utf8_encodes_a_high_cell_as_two_bytes() {
    // Column 300 -> code point 332 -> U+014C, two UTF-8 bytes 0xC5 0x8C.
    let mouse_report_bytes = encode_mouse(
        MouseKind::Press(MouseButton::Left),
        BindingModifierFlags::NONE,
        300,
        1,
        ANY_MOTION_TRACKING,
        MouseEncoding::Utf8,
    )
    .unwrap();
    assert_eq!(
        mouse_report_bytes,
        vec![0x1b, b'[', b'M', 32, 0xc5, 0x8c, 33]
    );
}

#[test]
fn urxvt_press_and_release_offset_the_button_code_by_32() {
    assert_eq!(
        encode_mouse(
            MouseKind::Press(MouseButton::Left),
            BindingModifierFlags::NONE,
            1,
            1,
            ANY_MOTION_TRACKING,
            MouseEncoding::Urxvt
        ),
        Some(b"\x1b[32;1;1M".to_vec()),
        "button code 0 offset by 32, decimal"
    );
    assert_eq!(
        encode_mouse(
            MouseKind::Release(MouseButton::Left),
            BindingModifierFlags::NONE,
            1,
            1,
            ANY_MOTION_TRACKING,
            MouseEncoding::Urxvt
        ),
        Some(b"\x1b[35;1;1M".to_vec()),
        "release reports 3, offset to 35"
    );
}

#[test]
fn urxvt_drag_motion_and_wheel_offset_the_whole_button_code() {
    // Left drag with shift: 0 + motion 32 + shift 4 = 36, offset to 68.
    assert_eq!(
        encode_mouse(
            MouseKind::Drag(MouseButton::Left),
            BindingModifierFlags::SHIFT,
            2,
            3,
            ANY_MOTION_TRACKING,
            MouseEncoding::Urxvt,
        ),
        Some(b"\x1b[68;2;3M".to_vec())
    );
    // Bare motion: 35, offset to 67.
    assert_eq!(
        encode_mouse(
            MouseKind::Motion,
            BindingModifierFlags::NONE,
            1,
            1,
            ANY_MOTION_TRACKING,
            MouseEncoding::Urxvt,
        ),
        Some(b"\x1b[67;1;1M".to_vec())
    );
    // Wheel up: 64, offset to 96.
    assert_eq!(
        encode_mouse(
            MouseKind::Scroll(ScrollDirection::Up),
            BindingModifierFlags::NONE,
            1,
            1,
            ANY_MOTION_TRACKING,
            MouseEncoding::Urxvt,
        ),
        Some(b"\x1b[96;1;1M".to_vec())
    );
}

#[test]
fn urxvt_writes_a_large_cell_in_decimal_without_a_cap() {
    assert_eq!(
        encode_mouse(
            MouseKind::Press(MouseButton::Left),
            BindingModifierFlags::NONE,
            300,
            u16::MAX,
            ANY_MOTION_TRACKING,
            MouseEncoding::Urxvt,
        ),
        Some(b"\x1b[32;300;65535M".to_vec())
    );
}

// --- Tracking ladder: what each level reports ------------------------------

#[test]
fn off_reports_nothing() {
    for mouse_kind in [
        MouseKind::Press(MouseButton::Left),
        MouseKind::Release(MouseButton::Left),
        MouseKind::Drag(MouseButton::Left),
        MouseKind::Motion,
        MouseKind::Scroll(ScrollDirection::Up),
    ] {
        assert_eq!(
            encode_mouse(
                mouse_kind,
                BindingModifierFlags::NONE,
                1,
                1,
                MouseTracking::Off,
                MouseEncoding::Sgr
            ),
            None,
            "{mouse_kind:?} is not reported when tracking is off"
        );
    }
}

#[test]
fn x10_reports_only_presses() {
    let encode_x10_mouse_event = |mouse_kind| {
        encode_mouse(
            mouse_kind,
            BindingModifierFlags::NONE,
            1,
            1,
            MouseTracking::X10,
            MouseEncoding::Sgr,
        )
    };
    assert_eq!(
        encode_x10_mouse_event(MouseKind::Press(MouseButton::Left)),
        Some(b"\x1b[<0;1;1M".to_vec())
    );
    assert_eq!(
        encode_x10_mouse_event(MouseKind::Scroll(ScrollDirection::Up)),
        None,
        "X10 does not report the wheel"
    );
    assert_eq!(
        encode_x10_mouse_event(MouseKind::Release(MouseButton::Left)),
        None
    );
    assert_eq!(
        encode_x10_mouse_event(MouseKind::Drag(MouseButton::Left)),
        None
    );
    assert_eq!(encode_x10_mouse_event(MouseKind::Motion), None);
}

#[test]
fn x10_drops_modifier_bits_under_sgr_encoding_too() {
    assert_eq!(
        encode_mouse(
            MouseKind::Press(MouseButton::Right),
            BindingModifierFlags::SHIFT.union(BindingModifierFlags::CTRL),
            4,
            5,
            MouseTracking::X10,
            MouseEncoding::Sgr,
        ),
        Some(b"\x1b[<2;4;5M".to_vec())
    );
}

#[test]
fn x10_omits_modifier_bits_that_higher_modes_carry() {
    // X10 (?9) reports only the button: a Ctrl+left press stays button 0.
    assert_eq!(
        encode_mouse(
            MouseKind::Press(MouseButton::Left),
            BindingModifierFlags::CTRL,
            1,
            1,
            MouseTracking::X10,
            MouseEncoding::Default,
        ),
        Some(vec![0x1b, b'[', b'M', 32, 33, 33]),
        "X10 drops the ctrl bit; the button code stays 0"
    );
    // Normal tracking keeps the modifier bit for the same click.
    assert_eq!(
        encode_mouse(
            MouseKind::Press(MouseButton::Left),
            BindingModifierFlags::CTRL,
            1,
            1,
            MouseTracking::Normal,
            MouseEncoding::Default,
        ),
        Some(vec![0x1b, b'[', b'M', 32 + 16, 33, 33]),
        "normal tracking adds ctrl = 16"
    );
}

#[test]
fn normal_adds_releases_but_not_motion() {
    let encode_normal_tracking_mouse_event = |mouse_kind| {
        encode_mouse(
            mouse_kind,
            BindingModifierFlags::NONE,
            1,
            1,
            MouseTracking::Normal,
            MouseEncoding::Sgr,
        )
    };
    assert_eq!(
        encode_normal_tracking_mouse_event(MouseKind::Press(MouseButton::Left)),
        Some(b"\x1b[<0;1;1M".to_vec())
    );
    assert_eq!(
        encode_normal_tracking_mouse_event(MouseKind::Release(MouseButton::Left)),
        Some(b"\x1b[<0;1;1m".to_vec())
    );
    assert_eq!(
        encode_normal_tracking_mouse_event(MouseKind::Scroll(ScrollDirection::Down)),
        Some(b"\x1b[<65;1;1M".to_vec()),
        "a wheel tick reports from normal tracking up"
    );
    assert_eq!(
        encode_normal_tracking_mouse_event(MouseKind::Drag(MouseButton::Left)),
        None
    );
    assert_eq!(encode_normal_tracking_mouse_event(MouseKind::Motion), None);
}

#[test]
fn button_motion_adds_drag_but_not_bare_motion() {
    let encode_button_motion_mouse_event = |mouse_kind| {
        encode_mouse(
            mouse_kind,
            BindingModifierFlags::NONE,
            1,
            1,
            MouseTracking::ButtonMotion,
            MouseEncoding::Sgr,
        )
    };
    assert_eq!(
        encode_button_motion_mouse_event(MouseKind::Drag(MouseButton::Left)),
        Some(b"\x1b[<32;1;1M".to_vec())
    );
    assert_eq!(encode_button_motion_mouse_event(MouseKind::Motion), None);
}

#[test]
fn any_motion_reports_bare_motion() {
    assert_eq!(
        encode_mouse(
            MouseKind::Motion,
            BindingModifierFlags::NONE,
            1,
            1,
            MouseTracking::AnyMotion,
            MouseEncoding::Sgr
        ),
        Some(b"\x1b[<35;1;1M".to_vec())
    );
}
