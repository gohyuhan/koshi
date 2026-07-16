//! Mouse-report encoding tests: exact bytes per encoding, the modifier and
//! motion bits, and the tracking-level ladder that decides what is reported.

use super::*;

const ANY: MouseTracking = MouseTracking::AnyMotion;

fn press(button: MouseButton) -> MouseKind {
    MouseKind::Press(button)
}

// --- SGR encoding: `CSI < cb ; col ; row M/m` ------------------------------

#[test]
fn sgr_left_press_and_release() {
    assert_eq!(
        encode_mouse(
            press(MouseButton::Left),
            ModFlags::NONE,
            1,
            1,
            ANY,
            MouseEncoding::Sgr
        ),
        Some(b"\x1b[<0;1;1M".to_vec())
    );
    assert_eq!(
        encode_mouse(
            MouseKind::Release(MouseButton::Left),
            ModFlags::NONE,
            1,
            1,
            ANY,
            MouseEncoding::Sgr
        ),
        Some(b"\x1b[<0;1;1m".to_vec()),
        "a release keeps the button and ends with a lowercase m"
    );
}

#[test]
fn sgr_button_numbers() {
    let cb = |button| {
        encode_mouse(press(button), ModFlags::NONE, 1, 1, ANY, MouseEncoding::Sgr).unwrap()
    };
    assert_eq!(cb(MouseButton::Left), b"\x1b[<0;1;1M");
    assert_eq!(cb(MouseButton::Middle), b"\x1b[<1;1;1M");
    assert_eq!(cb(MouseButton::Right), b"\x1b[<2;1;1M");
}

#[test]
fn sgr_modifier_bits_add_shift_alt_ctrl() {
    let with = |mods| {
        encode_mouse(
            press(MouseButton::Left),
            mods,
            1,
            1,
            ANY,
            MouseEncoding::Sgr,
        )
        .unwrap()
    };
    assert_eq!(with(ModFlags::SHIFT), b"\x1b[<4;1;1M");
    assert_eq!(with(ModFlags::ALT), b"\x1b[<8;1;1M");
    assert_eq!(with(ModFlags::CTRL), b"\x1b[<16;1;1M");
    assert_eq!(
        with(ModFlags::CTRL.union(ModFlags::SHIFT)),
        b"\x1b[<20;1;1M",
        "modifier bits sum"
    );
    assert_eq!(
        with(ModFlags::SUPER),
        b"\x1b[<0;1;1M",
        "super has no protocol bit"
    );
}

#[test]
fn sgr_drag_and_motion_set_the_motion_bit() {
    assert_eq!(
        encode_mouse(
            MouseKind::Drag(MouseButton::Left),
            ModFlags::NONE,
            2,
            3,
            ANY,
            MouseEncoding::Sgr
        ),
        Some(b"\x1b[<32;2;3M".to_vec()),
        "drag = button 0 + motion bit 32"
    );
    assert_eq!(
        encode_mouse(
            MouseKind::Motion,
            ModFlags::NONE,
            5,
            6,
            ANY,
            MouseEncoding::Sgr
        ),
        Some(b"\x1b[<35;5;6M".to_vec()),
        "bare motion = no-button 3 + motion bit 32"
    );
}

#[test]
fn sgr_wheel_directions() {
    let wheel = |direction| {
        encode_mouse(
            MouseKind::Scroll(direction),
            ModFlags::NONE,
            1,
            1,
            ANY,
            MouseEncoding::Sgr,
        )
        .unwrap()
    };
    assert_eq!(wheel(ScrollDirection::Up), b"\x1b[<64;1;1M");
    assert_eq!(wheel(ScrollDirection::Down), b"\x1b[<65;1;1M");
    assert_eq!(wheel(ScrollDirection::Left), b"\x1b[<66;1;1M");
    assert_eq!(wheel(ScrollDirection::Right), b"\x1b[<67;1;1M");
}

// --- Legacy / UTF-8 / urxvt encodings --------------------------------------

#[test]
fn legacy_press_and_release_bytes() {
    assert_eq!(
        encode_mouse(
            press(MouseButton::Left),
            ModFlags::NONE,
            1,
            1,
            ANY,
            MouseEncoding::Default
        ),
        Some(vec![0x1b, b'[', b'M', 32, 33, 33]),
        "cb 0, col 1, row 1, each offset by 32"
    );
    assert_eq!(
        encode_mouse(
            MouseKind::Release(MouseButton::Left),
            ModFlags::NONE,
            1,
            1,
            ANY,
            MouseEncoding::Default
        ),
        Some(vec![0x1b, b'[', b'M', 35, 33, 33]),
        "a legacy release loses the button and reports 3 (offset to 35)"
    );
}

#[test]
fn legacy_caps_a_cell_past_the_byte_limit() {
    let bytes = encode_mouse(
        press(MouseButton::Left),
        ModFlags::NONE,
        300,
        1,
        ANY,
        MouseEncoding::Default,
    )
    .unwrap();
    assert_eq!(
        bytes[4], 255,
        "column 300 would overflow a byte, so it saturates"
    );
}

#[test]
fn a_coordinate_near_u16_max_saturates_without_overflowing() {
    // `value + 32` must not overflow u16 before the byte cap: the legacy byte
    // saturates and the UTF-8 form does not panic.
    let legacy = encode_mouse(
        press(MouseButton::Left),
        ModFlags::NONE,
        u16::MAX,
        1,
        MouseTracking::Normal,
        MouseEncoding::Default,
    )
    .unwrap();
    assert_eq!(legacy[4], 255, "the column byte saturates at 255");

    assert!(
        encode_mouse(
            press(MouseButton::Left),
            ModFlags::NONE,
            u16::MAX,
            1,
            MouseTracking::Normal,
            MouseEncoding::Utf8,
        )
        .is_some(),
        "the UTF-8 form encodes a huge coordinate without panicking"
    );
}

#[test]
fn utf8_encodes_a_high_cell_as_two_bytes() {
    // Column 300 -> code point 332 -> U+014C, two UTF-8 bytes 0xC5 0x8C.
    let bytes = encode_mouse(
        press(MouseButton::Left),
        ModFlags::NONE,
        300,
        1,
        ANY,
        MouseEncoding::Utf8,
    )
    .unwrap();
    assert_eq!(bytes, vec![0x1b, b'[', b'M', 32, 0xc5, 0x8c, 33]);
}

#[test]
fn urxvt_press_and_release() {
    assert_eq!(
        encode_mouse(
            press(MouseButton::Left),
            ModFlags::NONE,
            1,
            1,
            ANY,
            MouseEncoding::Urxvt
        ),
        Some(b"\x1b[32;1;1M".to_vec()),
        "cb 0 offset by 32, decimal"
    );
    assert_eq!(
        encode_mouse(
            MouseKind::Release(MouseButton::Left),
            ModFlags::NONE,
            1,
            1,
            ANY,
            MouseEncoding::Urxvt
        ),
        Some(b"\x1b[35;1;1M".to_vec()),
        "release reports 3, offset to 35"
    );
}

// --- Tracking ladder: what each level reports ------------------------------

#[test]
fn off_reports_nothing() {
    for kind in [
        press(MouseButton::Left),
        MouseKind::Release(MouseButton::Left),
        MouseKind::Drag(MouseButton::Left),
        MouseKind::Motion,
        MouseKind::Scroll(ScrollDirection::Up),
    ] {
        assert_eq!(
            encode_mouse(
                kind,
                ModFlags::NONE,
                1,
                1,
                MouseTracking::Off,
                MouseEncoding::Sgr
            ),
            None,
            "{kind:?} is not reported when tracking is off"
        );
    }
}

#[test]
fn x10_reports_only_presses() {
    let at = |kind| {
        encode_mouse(
            kind,
            ModFlags::NONE,
            1,
            1,
            MouseTracking::X10,
            MouseEncoding::Sgr,
        )
    };
    assert!(at(press(MouseButton::Left)).is_some());
    assert!(
        at(MouseKind::Scroll(ScrollDirection::Up)).is_none(),
        "X10 predates the wheel"
    );
    assert!(at(MouseKind::Release(MouseButton::Left)).is_none());
    assert!(at(MouseKind::Drag(MouseButton::Left)).is_none());
    assert!(at(MouseKind::Motion).is_none());
}

#[test]
fn x10_omits_modifier_bits_that_later_modes_carry() {
    // X10 (?9) reports only the button: a Ctrl+left press stays button 0.
    assert_eq!(
        encode_mouse(
            press(MouseButton::Left),
            ModFlags::CTRL,
            1,
            1,
            MouseTracking::X10,
            MouseEncoding::Default,
        ),
        Some(vec![0x1b, b'[', b'M', 32, 33, 33]),
        "X10 drops the ctrl bit; Cb stays 0"
    );
    // Normal tracking keeps the modifier bit for the same click.
    assert_eq!(
        encode_mouse(
            press(MouseButton::Left),
            ModFlags::CTRL,
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
    let at = |kind| {
        encode_mouse(
            kind,
            ModFlags::NONE,
            1,
            1,
            MouseTracking::Normal,
            MouseEncoding::Sgr,
        )
    };
    assert!(at(press(MouseButton::Left)).is_some());
    assert!(at(MouseKind::Release(MouseButton::Left)).is_some());
    assert!(at(MouseKind::Drag(MouseButton::Left)).is_none());
    assert!(at(MouseKind::Motion).is_none());
}

#[test]
fn button_motion_adds_drag_but_not_bare_motion() {
    let at = |kind| {
        encode_mouse(
            kind,
            ModFlags::NONE,
            1,
            1,
            MouseTracking::ButtonMotion,
            MouseEncoding::Sgr,
        )
    };
    assert!(at(MouseKind::Drag(MouseButton::Left)).is_some());
    assert!(at(MouseKind::Motion).is_none());
}

#[test]
fn any_motion_reports_bare_motion() {
    assert!(encode_mouse(
        MouseKind::Motion,
        ModFlags::NONE,
        1,
        1,
        MouseTracking::AnyMotion,
        MouseEncoding::Sgr
    )
    .is_some());
}
