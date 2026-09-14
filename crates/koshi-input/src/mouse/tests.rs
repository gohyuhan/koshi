//! Mouse-boundary tests for host SGR events and Koshi cell coordinates.

use super::*;

use crate::host::{Event, Parser};
use koshi_core::geometry::Point;

/// Decode one SGR mouse sequence through the host parser boundary.
fn decode_mouse_bytes(mouse_bytes: &[u8]) -> Option<MouseInput> {
    let mut parser = Parser::default();
    parser.process_input_bytes(mouse_bytes);
    parser.finish_pending_input();
    let Some(Event::Mouse(mouse_event)) = parser.remove_next_pending_event() else {
        panic!("expected one mouse event from {mouse_bytes:?}");
    };
    assert_eq!(parser.remove_next_pending_event(), None);
    Some(decode_mouse(mouse_event))
}

fn build_mouse_input(
    mouse_kind: MouseKind,
    column: u16,
    row: u16,
    modifier_flags: ModFlags,
) -> Option<MouseInput> {
    Some(MouseInput {
        mouse_kind,
        position: Point { column, row },
        modifier_flags,
    })
}

#[test]
fn press_release_and_drag_carry_their_button() {
    assert_eq!(
        decode_mouse_bytes(b"\x1b[<0;11;4M"),
        build_mouse_input(MouseKind::Press(MouseButton::Left), 10, 3, ModFlags::NONE)
    );
    assert_eq!(
        decode_mouse_bytes(b"\x1b[<1;11;4m"),
        build_mouse_input(
            MouseKind::Release(MouseButton::Middle),
            10,
            3,
            ModFlags::NONE,
        )
    );
    assert_eq!(
        decode_mouse_bytes(b"\x1b[<34;11;4M"),
        build_mouse_input(MouseKind::Drag(MouseButton::Right), 10, 3, ModFlags::NONE)
    );
}

#[test]
fn every_mouse_button_maps_to_a_press() {
    assert_eq!(
        decode_mouse_bytes(b"\x1b[<0;2;2M")
            .expect("left")
            .mouse_kind,
        MouseKind::Press(MouseButton::Left)
    );
    assert_eq!(
        decode_mouse_bytes(b"\x1b[<1;2;2M")
            .expect("middle")
            .mouse_kind,
        MouseKind::Press(MouseButton::Middle)
    );
    assert_eq!(
        decode_mouse_bytes(b"\x1b[<2;2;2M")
            .expect("right")
            .mouse_kind,
        MouseKind::Press(MouseButton::Right)
    );
}

#[test]
fn every_scroll_direction_maps() {
    let scroll_direction_cases = [
        (64, ScrollDirection::Up),
        (65, ScrollDirection::Down),
        (66, ScrollDirection::Left),
        (67, ScrollDirection::Right),
    ];
    for (button_code, scroll_direction) in scroll_direction_cases {
        let mouse_sequence = format!("\x1b[<{button_code};11;4M");
        assert_eq!(
            decode_mouse_bytes(mouse_sequence.as_bytes())
                .expect("scroll")
                .mouse_kind,
            MouseKind::Scroll(scroll_direction),
            "button code {button_code}",
        );
    }
}

#[test]
fn buttonless_move_is_motion() {
    assert_eq!(
        decode_mouse_bytes(b"\x1b[<35;11;4M")
            .expect("motion")
            .mouse_kind,
        MouseKind::Motion
    );
}

#[test]
fn one_based_protocol_coordinates_become_zero_based_cells() {
    assert_eq!(
        decode_mouse_bytes(b"\x1b[<0;1;1M")
            .expect("origin")
            .position,
        Point { column: 0, row: 0 }
    );
    assert_eq!(
        decode_mouse_bytes(b"\x1b[<0;201;66M")
            .expect("cell")
            .position,
        Point {
            column: 200,
            row: 65,
        }
    );
}

#[test]
fn sgr_modifiers_map_individually_and_together() {
    assert_eq!(
        decode_mouse_bytes(b"\x1b[<4;2;2M")
            .expect("shift")
            .modifier_flags,
        ModFlags::SHIFT
    );
    assert_eq!(
        decode_mouse_bytes(b"\x1b[<8;2;2M")
            .expect("alt")
            .modifier_flags,
        ModFlags::ALT
    );
    assert_eq!(
        decode_mouse_bytes(b"\x1b[<16;2;2M")
            .expect("control")
            .modifier_flags,
        ModFlags::CTRL
    );
    assert_eq!(
        decode_mouse_bytes(b"\x1b[<28;2;2M")
            .expect("all")
            .modifier_flags,
        ModFlags::SHIFT.union(ModFlags::ALT).union(ModFlags::CTRL)
    );
}

#[test]
fn scroll_keeps_modifiers_and_position() {
    let scrolled = decode_mouse_bytes(b"\x1b[<80;5;3M").expect("control scroll");
    assert_eq!(scrolled.mouse_kind, MouseKind::Scroll(ScrollDirection::Up));
    assert_eq!(scrolled.modifier_flags, ModFlags::CTRL);
    assert_eq!(scrolled.position, Point { column: 4, row: 2 });
}

#[test]
fn host_super_and_meta_map_to_super_while_hyper_is_dropped() {
    let host_mouse_event = MouseEvent {
        mouse_event_kind: MouseEventKind::Moved,
        column: 7,
        row: 9,
        modifiers: Modifiers::SHIFT | Modifiers::SUPER | Modifiers::META | Modifiers::HYPER,
    };

    assert_eq!(
        decode_mouse(host_mouse_event),
        MouseInput {
            mouse_kind: MouseKind::Motion,
            position: Point { column: 7, row: 9 },
            modifier_flags: ModFlags::SHIFT | ModFlags::SUPER,
        }
    );
}

#[test]
fn full_u16_protocol_coordinates_rebase_without_overflow() {
    let mouse_input = decode_mouse_bytes(b"\x1b[<0;65535;65535M").expect("maximum coordinate");
    assert_eq!(
        mouse_input.position,
        Point {
            column: u16::MAX - 1,
            row: u16::MAX - 1,
        }
    );
}
