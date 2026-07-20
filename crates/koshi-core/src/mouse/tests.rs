//! Tests for the mouse vocabulary types.
//!
//! [`MouseButton`] and [`ScrollDirection`] are the only serde types here; their
//! wire form is a contract, since decoded mouse events serialize with these
//! values. [`MouseKind`] and [`MouseInput`] carry no serde impl, so they are
//! checked only for construction and equality.

use super::*;
use crate::geometry::Point;
use crate::key::ModFlags;

#[test]
fn mouse_button_serde_wire_form_is_the_variant_name() {
    assert_eq!(
        serde_json::to_string(&MouseButton::Left).expect("serialize"),
        "\"Left\""
    );
    assert_eq!(
        serde_json::to_string(&MouseButton::Middle).expect("serialize"),
        "\"Middle\""
    );
    assert_eq!(
        serde_json::to_string(&MouseButton::Right).expect("serialize"),
        "\"Right\""
    );
}

#[test]
fn scroll_direction_serde_wire_form_is_the_variant_name() {
    assert_eq!(
        serde_json::to_string(&ScrollDirection::Up).expect("serialize"),
        "\"Up\""
    );
    assert_eq!(
        serde_json::to_string(&ScrollDirection::Down).expect("serialize"),
        "\"Down\""
    );
    assert_eq!(
        serde_json::to_string(&ScrollDirection::Left).expect("serialize"),
        "\"Left\""
    );
    assert_eq!(
        serde_json::to_string(&ScrollDirection::Right).expect("serialize"),
        "\"Right\""
    );
}

#[test]
fn a_mouse_button_survives_a_serde_round_trip() {
    for button in [MouseButton::Left, MouseButton::Middle, MouseButton::Right] {
        let json = serde_json::to_string(&button).expect("serialize");
        let restored: MouseButton = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(button, restored);
    }
}

#[test]
fn a_scroll_direction_survives_a_serde_round_trip() {
    for direction in [
        ScrollDirection::Up,
        ScrollDirection::Down,
        ScrollDirection::Left,
        ScrollDirection::Right,
    ] {
        let json = serde_json::to_string(&direction).expect("serialize");
        let restored: ScrollDirection = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(direction, restored);
    }
}

#[test]
fn a_left_click_input_carries_its_kind_cell_and_modifiers() {
    let click = MouseInput {
        kind: MouseKind::Press(MouseButton::Left),
        at: Point { x: 10, y: 3 },
        mods: ModFlags::NONE,
    };

    assert_eq!(click.kind, MouseKind::Press(MouseButton::Left));
    assert_eq!(click.at, Point { x: 10, y: 3 });
    assert_eq!(click.mods, ModFlags::NONE);
}

#[test]
fn a_press_and_a_release_of_the_same_button_are_distinct_kinds() {
    assert_ne!(
        MouseKind::Press(MouseButton::Left),
        MouseKind::Release(MouseButton::Left)
    );
    assert_ne!(
        MouseKind::Press(MouseButton::Left),
        MouseKind::Press(MouseButton::Right)
    );
    assert_ne!(
        MouseKind::Scroll(ScrollDirection::Up),
        MouseKind::Scroll(ScrollDirection::Down)
    );
}
