//! Tests for the mouse vocabulary types.
//!
//! Every type here is a serde type and its wire form is a contract: a decoded
//! mouse event travels to the session as a [`MouseInput`], the session answers
//! with a [`MouseAnswer`], and a painted frame carries each pane's
//! [`MouseTracking`] level.

use super::*;
use crate::geometry::Point;
use crate::ids::PaneId;
use crate::key::ModFlags;
use uuid::Uuid;

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
fn mouse_tracking_serde_wire_form_is_the_variant_name() {
    assert_eq!(
        serde_json::to_string(&MouseTracking::Off).expect("serialize"),
        "\"Off\""
    );
    assert_eq!(
        serde_json::to_string(&MouseTracking::X10).expect("serialize"),
        "\"X10\""
    );
    assert_eq!(
        serde_json::to_string(&MouseTracking::Normal).expect("serialize"),
        "\"Normal\""
    );
    assert_eq!(
        serde_json::to_string(&MouseTracking::ButtonMotion).expect("serialize"),
        "\"ButtonMotion\""
    );
    assert_eq!(
        serde_json::to_string(&MouseTracking::AnyMotion).expect("serialize"),
        "\"AnyMotion\""
    );
}

#[test]
fn mouse_kind_serde_wire_form_names_the_variant_and_its_value() {
    assert_eq!(
        serde_json::to_string(&MouseKind::Press(MouseButton::Left)).expect("serialize"),
        "{\"Press\":\"Left\"}"
    );
    assert_eq!(
        serde_json::to_string(&MouseKind::Release(MouseButton::Right)).expect("serialize"),
        "{\"Release\":\"Right\"}"
    );
    assert_eq!(
        serde_json::to_string(&MouseKind::Drag(MouseButton::Middle)).expect("serialize"),
        "{\"Drag\":\"Middle\"}"
    );
    assert_eq!(
        serde_json::to_string(&MouseKind::Scroll(ScrollDirection::Up)).expect("serialize"),
        "{\"Scroll\":\"Up\"}"
    );
    assert_eq!(
        serde_json::to_string(&MouseKind::Motion).expect("serialize"),
        "\"Motion\""
    );
}

#[test]
fn mouse_input_serde_wire_form_carries_the_kind_cell_and_modifiers() {
    let click = MouseInput {
        kind: MouseKind::Press(MouseButton::Left),
        at: Point { x: 10, y: 3 },
        mods: ModFlags::CTRL,
    };

    assert_eq!(
        serde_json::to_string(&click).expect("serialize"),
        "{\"kind\":{\"Press\":\"Left\"},\"at\":{\"x\":10,\"y\":3},\"mods\":1}"
    );
}

#[test]
fn mouse_answer_serde_wire_form_names_the_variant_and_its_fields() {
    let pane = PaneId::from_uuid(Uuid::nil());

    assert_eq!(
        serde_json::to_string(&MouseAnswer::Scrolled {
            pane,
            top: Some(101)
        })
        .expect("serialize"),
        "{\"Scrolled\":{\"pane\":\"00000000-0000-0000-0000-000000000000\",\"top\":101}}"
    );
    assert_eq!(
        serde_json::to_string(&MouseAnswer::Scrolled { pane, top: None }).expect("serialize"),
        "{\"Scrolled\":{\"pane\":\"00000000-0000-0000-0000-000000000000\",\"top\":null}}"
    );
    assert_eq!(
        serde_json::to_string(&MouseAnswer::Resized {
            pane,
            side: Direction::Up,
            step: -1,
            applied: 3
        })
        .expect("serialize"),
        "{\"Resized\":{\"pane\":\"00000000-0000-0000-0000-000000000000\",\
         \"side\":\"Up\",\"step\":-1,\"applied\":3}}"
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
fn a_mouse_tracking_level_survives_a_serde_round_trip() {
    for tracking in [
        MouseTracking::Off,
        MouseTracking::X10,
        MouseTracking::Normal,
        MouseTracking::ButtonMotion,
        MouseTracking::AnyMotion,
    ] {
        let json = serde_json::to_string(&tracking).expect("serialize");
        let restored: MouseTracking = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(tracking, restored);
    }
}

#[test]
fn a_mouse_kind_survives_a_serde_round_trip() {
    for kind in [
        MouseKind::Press(MouseButton::Left),
        MouseKind::Release(MouseButton::Middle),
        MouseKind::Drag(MouseButton::Right),
        MouseKind::Scroll(ScrollDirection::Down),
        MouseKind::Motion,
    ] {
        let json = serde_json::to_string(&kind).expect("serialize");
        let restored: MouseKind = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(kind, restored);
    }
}

#[test]
fn a_mouse_input_survives_a_serde_round_trip() {
    let input = MouseInput {
        kind: MouseKind::Drag(MouseButton::Left),
        at: Point { x: 42, y: 7 },
        mods: ModFlags::CTRL.union(ModFlags::SHIFT),
    };

    let json = serde_json::to_string(&input).expect("serialize");
    let restored: MouseInput = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(input, restored);
}

#[test]
fn a_mouse_answer_survives_a_serde_round_trip() {
    let pane = PaneId::new();
    for answer in [
        MouseAnswer::Scrolled {
            pane,
            top: Some(101),
        },
        MouseAnswer::Scrolled { pane, top: None },
        MouseAnswer::Resized {
            pane,
            side: Direction::Up,
            step: -1,
            applied: 3,
        },
        MouseAnswer::Resized {
            pane,
            side: Direction::Right,
            step: 1,
            applied: 0,
        },
    ] {
        let json = serde_json::to_string(&answer).expect("serialize");
        let restored: MouseAnswer = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(answer, restored);
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

#[test]
fn every_tracking_level_answers_every_event_kind_exactly() {
    let press = MouseKind::Press(MouseButton::Left);
    let release = MouseKind::Release(MouseButton::Left);
    let drag = MouseKind::Drag(MouseButton::Left);
    let scroll = MouseKind::Scroll(ScrollDirection::Up);
    let motion = MouseKind::Motion;

    let table = [
        (MouseTracking::Off, press, false),
        (MouseTracking::Off, release, false),
        (MouseTracking::Off, drag, false),
        (MouseTracking::Off, scroll, false),
        (MouseTracking::Off, motion, false),
        (MouseTracking::X10, press, true),
        (MouseTracking::X10, release, false),
        (MouseTracking::X10, drag, false),
        (MouseTracking::X10, scroll, false),
        (MouseTracking::X10, motion, false),
        (MouseTracking::Normal, press, true),
        (MouseTracking::Normal, release, true),
        (MouseTracking::Normal, drag, false),
        (MouseTracking::Normal, scroll, true),
        (MouseTracking::Normal, motion, false),
        (MouseTracking::ButtonMotion, press, true),
        (MouseTracking::ButtonMotion, release, true),
        (MouseTracking::ButtonMotion, drag, true),
        (MouseTracking::ButtonMotion, scroll, true),
        (MouseTracking::ButtonMotion, motion, false),
        (MouseTracking::AnyMotion, press, true),
        (MouseTracking::AnyMotion, release, true),
        (MouseTracking::AnyMotion, drag, true),
        (MouseTracking::AnyMotion, scroll, true),
        (MouseTracking::AnyMotion, motion, true),
    ];

    for (tracking, kind, expected) in table {
        assert_eq!(
            reports(tracking, kind),
            expected,
            "{tracking:?} + {kind:?} must report {expected}"
        );
    }
}

#[test]
fn the_answer_is_the_same_for_every_button_and_every_scroll_direction() {
    for button in [MouseButton::Left, MouseButton::Middle, MouseButton::Right] {
        assert!(reports(MouseTracking::X10, MouseKind::Press(button)));
        assert!(!reports(MouseTracking::X10, MouseKind::Release(button)));
        assert!(reports(
            MouseTracking::ButtonMotion,
            MouseKind::Drag(button)
        ));
        assert!(!reports(MouseTracking::Normal, MouseKind::Drag(button)));
    }
    for direction in [
        ScrollDirection::Up,
        ScrollDirection::Down,
        ScrollDirection::Left,
        ScrollDirection::Right,
    ] {
        assert!(!reports(MouseTracking::X10, MouseKind::Scroll(direction)));
        assert!(reports(MouseTracking::Normal, MouseKind::Scroll(direction)));
    }
}
