//! Mouse-boundary tests for host SGR events and Koshi cell coordinates.

use super::*;

use crate::host::{Event, Parser};
use koshi_core::geometry::Point;

/// Decode one SGR mouse sequence through the host parser boundary.
fn decode(bytes: &[u8]) -> Option<MouseInput> {
    let mut parser = Parser::default();
    parser.push(bytes);
    parser.finish_pending();
    let Some(Event::Mouse(event)) = parser.pop() else {
        panic!("expected one mouse event from {bytes:?}");
    };
    assert_eq!(parser.pop(), None);
    Some(decode_mouse(event))
}

fn input(kind: MouseKind, x: u16, y: u16, mods: ModFlags) -> Option<MouseInput> {
    Some(MouseInput {
        kind,
        at: Point { x, y },
        mods,
    })
}

#[test]
fn press_release_and_drag_carry_their_button() {
    assert_eq!(
        decode(b"\x1b[<0;11;4M"),
        input(MouseKind::Press(MouseButton::Left), 10, 3, ModFlags::NONE)
    );
    assert_eq!(
        decode(b"\x1b[<1;11;4m"),
        input(
            MouseKind::Release(MouseButton::Middle),
            10,
            3,
            ModFlags::NONE,
        )
    );
    assert_eq!(
        decode(b"\x1b[<34;11;4M"),
        input(MouseKind::Drag(MouseButton::Right), 10, 3, ModFlags::NONE)
    );
}

#[test]
fn every_button_maps() {
    assert_eq!(
        decode(b"\x1b[<0;2;2M").expect("left").kind,
        MouseKind::Press(MouseButton::Left)
    );
    assert_eq!(
        decode(b"\x1b[<1;2;2M").expect("middle").kind,
        MouseKind::Press(MouseButton::Middle)
    );
    assert_eq!(
        decode(b"\x1b[<2;2;2M").expect("right").kind,
        MouseKind::Press(MouseButton::Right)
    );
}

#[test]
fn every_scroll_direction_maps() {
    let cases = [
        (64, ScrollDirection::Up),
        (65, ScrollDirection::Down),
        (66, ScrollDirection::Left),
        (67, ScrollDirection::Right),
    ];
    for (code, direction) in cases {
        let sequence = format!("\x1b[<{code};11;4M");
        assert_eq!(
            decode(sequence.as_bytes()).expect("scroll").kind,
            MouseKind::Scroll(direction),
            "button code {code}",
        );
    }
}

#[test]
fn buttonless_move_is_motion() {
    assert_eq!(
        decode(b"\x1b[<35;11;4M").expect("motion").kind,
        MouseKind::Motion
    );
}

#[test]
fn one_based_protocol_coordinates_become_zero_based_cells() {
    assert_eq!(
        decode(b"\x1b[<0;1;1M").expect("origin").at,
        Point { x: 0, y: 0 }
    );
    assert_eq!(
        decode(b"\x1b[<0;201;66M").expect("cell").at,
        Point { x: 200, y: 65 }
    );
}

#[test]
fn sgr_modifiers_map_individually_and_together() {
    assert_eq!(
        decode(b"\x1b[<4;2;2M").expect("shift").mods,
        ModFlags::SHIFT
    );
    assert_eq!(decode(b"\x1b[<8;2;2M").expect("alt").mods, ModFlags::ALT);
    assert_eq!(
        decode(b"\x1b[<16;2;2M").expect("control").mods,
        ModFlags::CTRL
    );
    assert_eq!(
        decode(b"\x1b[<28;2;2M").expect("all").mods,
        ModFlags::SHIFT.union(ModFlags::ALT).union(ModFlags::CTRL)
    );
}

#[test]
fn scroll_keeps_modifiers_and_position() {
    let scrolled = decode(b"\x1b[<80;5;3M").expect("control scroll");
    assert_eq!(scrolled.kind, MouseKind::Scroll(ScrollDirection::Up));
    assert_eq!(scrolled.mods, ModFlags::CTRL);
    assert_eq!(scrolled.at, Point { x: 4, y: 2 });
}

#[test]
fn full_u16_protocol_coordinates_rebase_without_overflow() {
    let event = decode(b"\x1b[<0;65535;65535M").expect("maximum coordinate");
    assert_eq!(
        event.at,
        Point {
            x: u16::MAX - 1,
            y: u16::MAX - 1,
        }
    );
}
