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
    let left_click = MouseInput {
        mouse_kind: MouseKind::Press(MouseButton::Left),
        position: Point { column: 10, row: 3 },
        modifier_flags: ModFlags::CTRL,
    };

    assert_eq!(
        serde_json::to_string(&left_click).expect("serialize"),
        "{\"kind\":{\"Press\":\"Left\"},\"at\":{\"x\":10,\"y\":3},\"mods\":1}"
    );
}

#[test]
fn mouse_answer_serde_wire_form_names_the_variant_and_its_fields() {
    let pane_id = PaneId::from_uuid(Uuid::nil());

    assert_eq!(
        serde_json::to_string(&MouseAnswer::Scrolled {
            pane_id,
            top_row_number: Some(101)
        })
        .expect("serialize"),
        "{\"Scrolled\":{\"pane\":\"00000000-0000-0000-0000-000000000000\",\"top\":101}}"
    );
    assert_eq!(
        serde_json::to_string(&MouseAnswer::Scrolled {
            pane_id,
            top_row_number: None,
        })
        .expect("serialize"),
        "{\"Scrolled\":{\"pane\":\"00000000-0000-0000-0000-000000000000\",\"top\":null}}"
    );
    assert_eq!(
        serde_json::to_string(&MouseAnswer::Resized {
            pane_id,
            border_side: Direction::Up,
            resize_step: -1,
            applied_cell_count: 3
        })
        .expect("serialize"),
        "{\"Resized\":{\"pane\":\"00000000-0000-0000-0000-000000000000\",\
         \"side\":\"Up\",\"step\":-1,\"applied\":3}}"
    );
}

#[test]
fn a_mouse_button_survives_a_serde_round_trip() {
    for mouse_button in [MouseButton::Left, MouseButton::Middle, MouseButton::Right] {
        let mouse_button_json = serde_json::to_string(&mouse_button).expect("serialize");
        let deserialized_mouse_button: MouseButton =
            serde_json::from_str(&mouse_button_json).expect("deserialize");
        assert_eq!(mouse_button, deserialized_mouse_button);
    }
}

#[test]
fn a_scroll_direction_survives_a_serde_round_trip() {
    for scroll_direction in [
        ScrollDirection::Up,
        ScrollDirection::Down,
        ScrollDirection::Left,
        ScrollDirection::Right,
    ] {
        let scroll_direction_json = serde_json::to_string(&scroll_direction).expect("serialize");
        let deserialized_scroll_direction: ScrollDirection =
            serde_json::from_str(&scroll_direction_json).expect("deserialize");
        assert_eq!(scroll_direction, deserialized_scroll_direction);
    }
}

#[test]
fn a_mouse_tracking_level_survives_a_serde_round_trip() {
    for mouse_tracking in [
        MouseTracking::Off,
        MouseTracking::X10,
        MouseTracking::Normal,
        MouseTracking::ButtonMotion,
        MouseTracking::AnyMotion,
    ] {
        let mouse_tracking_json = serde_json::to_string(&mouse_tracking).expect("serialize");
        let deserialized_mouse_tracking: MouseTracking =
            serde_json::from_str(&mouse_tracking_json).expect("deserialize");
        assert_eq!(mouse_tracking, deserialized_mouse_tracking);
    }
}

#[test]
fn a_mouse_kind_survives_a_serde_round_trip() {
    for mouse_kind in [
        MouseKind::Press(MouseButton::Left),
        MouseKind::Release(MouseButton::Middle),
        MouseKind::Drag(MouseButton::Right),
        MouseKind::Scroll(ScrollDirection::Down),
        MouseKind::Motion,
    ] {
        let mouse_kind_json = serde_json::to_string(&mouse_kind).expect("serialize");
        let deserialized_mouse_kind: MouseKind =
            serde_json::from_str(&mouse_kind_json).expect("deserialize");
        assert_eq!(mouse_kind, deserialized_mouse_kind);
    }
}

#[test]
fn a_mouse_input_survives_a_serde_round_trip() {
    let mouse_input = MouseInput {
        mouse_kind: MouseKind::Drag(MouseButton::Left),
        position: Point { column: 42, row: 7 },
        modifier_flags: ModFlags::CTRL.union(ModFlags::SHIFT),
    };

    let mouse_input_json = serde_json::to_string(&mouse_input).expect("serialize");
    let deserialized_mouse_input: MouseInput =
        serde_json::from_str(&mouse_input_json).expect("deserialize");
    assert_eq!(mouse_input, deserialized_mouse_input);
}

#[test]
fn a_mouse_answer_survives_a_serde_round_trip() {
    let pane_id = PaneId::new();
    for mouse_answer in [
        MouseAnswer::Scrolled {
            pane_id,
            top_row_number: Some(101),
        },
        MouseAnswer::Scrolled {
            pane_id,
            top_row_number: None,
        },
        MouseAnswer::Resized {
            pane_id,
            border_side: Direction::Up,
            resize_step: -1,
            applied_cell_count: 3,
        },
        MouseAnswer::Resized {
            pane_id,
            border_side: Direction::Right,
            resize_step: 1,
            applied_cell_count: 0,
        },
    ] {
        let mouse_answer_json = serde_json::to_string(&mouse_answer).expect("serialize");
        let deserialized_mouse_answer: MouseAnswer =
            serde_json::from_str(&mouse_answer_json).expect("deserialize");
        assert_eq!(mouse_answer, deserialized_mouse_answer);
    }
}

#[test]
fn a_left_click_input_carries_its_kind_cell_and_modifiers() {
    let left_click = MouseInput {
        mouse_kind: MouseKind::Press(MouseButton::Left),
        position: Point { column: 10, row: 3 },
        modifier_flags: ModFlags::NONE,
    };

    assert_eq!(left_click.mouse_kind, MouseKind::Press(MouseButton::Left));
    assert_eq!(left_click.position, Point { column: 10, row: 3 });
    assert_eq!(left_click.modifier_flags, ModFlags::NONE);
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
    let press_mouse_kind = MouseKind::Press(MouseButton::Left);
    let release_mouse_kind = MouseKind::Release(MouseButton::Left);
    let drag_mouse_kind = MouseKind::Drag(MouseButton::Left);
    let scroll_mouse_kind = MouseKind::Scroll(ScrollDirection::Up);
    let motion_mouse_kind = MouseKind::Motion;

    let tracking_expectations = [
        (MouseTracking::Off, press_mouse_kind, false),
        (MouseTracking::Off, release_mouse_kind, false),
        (MouseTracking::Off, drag_mouse_kind, false),
        (MouseTracking::Off, scroll_mouse_kind, false),
        (MouseTracking::Off, motion_mouse_kind, false),
        (MouseTracking::X10, press_mouse_kind, true),
        (MouseTracking::X10, release_mouse_kind, false),
        (MouseTracking::X10, drag_mouse_kind, false),
        (MouseTracking::X10, scroll_mouse_kind, false),
        (MouseTracking::X10, motion_mouse_kind, false),
        (MouseTracking::Normal, press_mouse_kind, true),
        (MouseTracking::Normal, release_mouse_kind, true),
        (MouseTracking::Normal, drag_mouse_kind, false),
        (MouseTracking::Normal, scroll_mouse_kind, true),
        (MouseTracking::Normal, motion_mouse_kind, false),
        (MouseTracking::ButtonMotion, press_mouse_kind, true),
        (MouseTracking::ButtonMotion, release_mouse_kind, true),
        (MouseTracking::ButtonMotion, drag_mouse_kind, true),
        (MouseTracking::ButtonMotion, scroll_mouse_kind, true),
        (MouseTracking::ButtonMotion, motion_mouse_kind, false),
        (MouseTracking::AnyMotion, press_mouse_kind, true),
        (MouseTracking::AnyMotion, release_mouse_kind, true),
        (MouseTracking::AnyMotion, drag_mouse_kind, true),
        (MouseTracking::AnyMotion, scroll_mouse_kind, true),
        (MouseTracking::AnyMotion, motion_mouse_kind, true),
    ];

    for (mouse_tracking, mouse_kind, expected_is_reported) in tracking_expectations {
        assert_eq!(
            is_mouse_kind_reported(mouse_tracking, mouse_kind),
            expected_is_reported,
            "{mouse_tracking:?} + {mouse_kind:?} must report {expected_is_reported}"
        );
    }
}

#[test]
fn the_answer_is_the_same_for_every_button_and_every_scroll_direction() {
    for mouse_button in [MouseButton::Left, MouseButton::Middle, MouseButton::Right] {
        assert!(is_mouse_kind_reported(
            MouseTracking::X10,
            MouseKind::Press(mouse_button)
        ));
        assert!(!is_mouse_kind_reported(
            MouseTracking::X10,
            MouseKind::Release(mouse_button)
        ));
        assert!(is_mouse_kind_reported(
            MouseTracking::ButtonMotion,
            MouseKind::Drag(mouse_button)
        ));
        assert!(!is_mouse_kind_reported(
            MouseTracking::Normal,
            MouseKind::Drag(mouse_button)
        ));
    }
    for scroll_direction in [
        ScrollDirection::Up,
        ScrollDirection::Down,
        ScrollDirection::Left,
        ScrollDirection::Right,
    ] {
        assert!(!is_mouse_kind_reported(
            MouseTracking::X10,
            MouseKind::Scroll(scroll_direction)
        ));
        assert!(is_mouse_kind_reported(
            MouseTracking::Normal,
            MouseKind::Scroll(scroll_direction)
        ));
    }
}

#[test]
fn mouse_tracking_default_is_off() {
    assert_eq!(MouseTracking::default(), MouseTracking::Off);
}

#[test]
fn decoding_refuses_a_mouse_kind_no_variant_names() {
    let mouse_kind_parse_error =
        serde_json::from_str::<MouseKind>("\"Hover\"").expect_err("no such kind");
    assert_eq!(
        mouse_kind_parse_error.to_string(),
        "unknown variant `Hover`, expected one of `Press`, `Release`, `Drag`, `Scroll`, `Motion` \
         at line 1 column 7"
    );
}

#[test]
fn a_scrolled_answer_missing_its_top_line_decodes_as_none() {
    let decoded_mouse_answer: MouseAnswer =
        serde_json::from_str("{\"Scrolled\":{\"pane\":\"00000000-0000-0000-0000-000000000000\"}}")
            .expect("a missing `top` reads as `None`");
    assert_eq!(
        decoded_mouse_answer,
        MouseAnswer::Scrolled {
            pane_id: PaneId::from_uuid(Uuid::nil()),
            top_row_number: None
        }
    );
}
