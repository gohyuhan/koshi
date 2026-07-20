//! Mouse-boundary tests: the decode table (host event → canonical
//! [`MouseInput`]), covering every event kind, each button, all four scroll
//! directions, bare motion, coordinate pass-through, and every modifier.

use super::*;
use crossterm::event::{KeyModifiers, MouseButton as HostButton, MouseEvent, MouseEventKind};
use koshi_core::geometry::Point;

/// One host mouse event at a cell with the given modifiers.
fn ev(kind: MouseEventKind, column: u16, row: u16, modifiers: KeyModifiers) -> MouseEvent {
    MouseEvent {
        kind,
        column,
        row,
        modifiers,
    }
}

/// The event at column 10, row 3 with nothing held — the fixed cell the kind
/// tests reuse, so each case shows only the kind that changed.
fn at_10_3(kind: MouseEventKind) -> MouseInput {
    decode_mouse(ev(kind, 10, 3, KeyModifiers::NONE))
}

fn input(kind: MouseKind, x: u16, y: u16, mods: ModFlags) -> MouseInput {
    MouseInput {
        kind,
        at: Point { x, y },
        mods,
    }
}

// ------------------------------------------------------------- buttons ----

#[test]
fn press_release_drag_carry_their_button() {
    assert_eq!(
        at_10_3(MouseEventKind::Down(HostButton::Left)),
        input(MouseKind::Press(MouseButton::Left), 10, 3, ModFlags::NONE)
    );
    assert_eq!(
        at_10_3(MouseEventKind::Up(HostButton::Middle)),
        input(
            MouseKind::Release(MouseButton::Middle),
            10,
            3,
            ModFlags::NONE
        )
    );
    assert_eq!(
        at_10_3(MouseEventKind::Drag(HostButton::Right)),
        input(MouseKind::Drag(MouseButton::Right), 10, 3, ModFlags::NONE)
    );
}

#[test]
fn every_button_maps() {
    assert_eq!(
        at_10_3(MouseEventKind::Down(HostButton::Left)).kind,
        MouseKind::Press(MouseButton::Left)
    );
    assert_eq!(
        at_10_3(MouseEventKind::Down(HostButton::Middle)).kind,
        MouseKind::Press(MouseButton::Middle)
    );
    assert_eq!(
        at_10_3(MouseEventKind::Down(HostButton::Right)).kind,
        MouseKind::Press(MouseButton::Right)
    );
}

// ------------------------------------------------------------- scroll -----

#[test]
fn every_scroll_direction_maps() {
    assert_eq!(
        at_10_3(MouseEventKind::ScrollUp).kind,
        MouseKind::Scroll(ScrollDirection::Up)
    );
    assert_eq!(
        at_10_3(MouseEventKind::ScrollDown).kind,
        MouseKind::Scroll(ScrollDirection::Down)
    );
    assert_eq!(
        at_10_3(MouseEventKind::ScrollLeft).kind,
        MouseKind::Scroll(ScrollDirection::Left)
    );
    assert_eq!(
        at_10_3(MouseEventKind::ScrollRight).kind,
        MouseKind::Scroll(ScrollDirection::Right)
    );
}

// ------------------------------------------------------------- motion -----

#[test]
fn buttonless_move_is_motion() {
    assert_eq!(at_10_3(MouseEventKind::Moved).kind, MouseKind::Motion);
}

// -------------------------------------------------------- coordinates -----

#[test]
fn coordinates_pass_through_unchanged() {
    assert_eq!(
        decode_mouse(ev(
            MouseEventKind::Down(HostButton::Left),
            0,
            0,
            KeyModifiers::NONE
        ))
        .at,
        Point { x: 0, y: 0 }
    );
    assert_eq!(
        decode_mouse(ev(
            MouseEventKind::Down(HostButton::Left),
            200,
            65,
            KeyModifiers::NONE
        ))
        .at,
        Point { x: 200, y: 65 }
    );
}

// ----------------------------------------------------------- modifiers ----

#[test]
fn each_modifier_maps() {
    let kind = MouseEventKind::Down(HostButton::Left);
    assert_eq!(
        decode_mouse(ev(kind, 1, 1, KeyModifiers::CONTROL)).mods,
        ModFlags::CTRL
    );
    assert_eq!(
        decode_mouse(ev(kind, 1, 1, KeyModifiers::ALT)).mods,
        ModFlags::ALT
    );
    assert_eq!(
        decode_mouse(ev(kind, 1, 1, KeyModifiers::SHIFT)).mods,
        ModFlags::SHIFT
    );
    assert_eq!(
        decode_mouse(ev(kind, 1, 1, KeyModifiers::SUPER)).mods,
        ModFlags::SUPER
    );
}

#[test]
fn meta_maps_to_super_like_the_keyboard_boundary() {
    assert_eq!(
        decode_mouse(ev(
            MouseEventKind::Down(HostButton::Left),
            1,
            1,
            KeyModifiers::META
        ))
        .mods,
        ModFlags::SUPER
    );
}

#[test]
fn combined_modifiers_all_land() {
    let mods = KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT;
    assert_eq!(
        decode_mouse(ev(MouseEventKind::Drag(HostButton::Left), 5, 5, mods)).mods,
        ModFlags::CTRL.union(ModFlags::ALT).union(ModFlags::SHIFT)
    );
}

#[test]
fn all_four_modifiers_land_on_one_event() {
    let mods =
        KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT | KeyModifiers::SUPER;
    assert_eq!(
        decode_mouse(ev(MouseEventKind::Down(HostButton::Left), 5, 5, mods)).mods,
        ModFlags::CTRL
            .union(ModFlags::ALT)
            .union(ModFlags::SHIFT)
            .union(ModFlags::SUPER)
    );
}

// ------------------------------------------- buttons on release and drag ----

#[test]
fn release_carries_every_button() {
    assert_eq!(
        at_10_3(MouseEventKind::Up(HostButton::Left)).kind,
        MouseKind::Release(MouseButton::Left)
    );
    assert_eq!(
        at_10_3(MouseEventKind::Up(HostButton::Right)).kind,
        MouseKind::Release(MouseButton::Right)
    );
}

#[test]
fn drag_carries_every_button() {
    assert_eq!(
        at_10_3(MouseEventKind::Drag(HostButton::Left)).kind,
        MouseKind::Drag(MouseButton::Left)
    );
    assert_eq!(
        at_10_3(MouseEventKind::Drag(HostButton::Middle)).kind,
        MouseKind::Drag(MouseButton::Middle)
    );
}

// ------------------------------------------- kind and modifiers together ----

#[test]
fn a_scroll_carries_the_modifiers_held_with_it() {
    // Ctrl+wheel is a common zoom gesture; the modifier must survive decode
    // alongside the direction.
    let scrolled = decode_mouse(ev(MouseEventKind::ScrollUp, 4, 2, KeyModifiers::CONTROL));
    assert_eq!(scrolled.kind, MouseKind::Scroll(ScrollDirection::Up));
    assert_eq!(scrolled.mods, ModFlags::CTRL);
    assert_eq!(scrolled.at, Point { x: 4, y: 2 });
}

#[test]
fn a_bare_motion_carries_its_modifiers() {
    let moved = decode_mouse(ev(MouseEventKind::Moved, 7, 8, KeyModifiers::ALT));
    assert_eq!(moved.kind, MouseKind::Motion);
    assert_eq!(moved.mods, ModFlags::ALT);
}

// -------------------------------------------------- coordinate extremes ----

#[test]
fn coordinates_pass_through_at_the_edges_of_the_range() {
    // The legacy protocol capped a coordinate at 223; SGR mouse reporting runs
    // to the full `u16`. The decoder copies whatever the host gives — 223, the
    // cell just past it, and the top of the range all pass through unchanged.
    for coord in [223u16, 224, u16::MAX] {
        assert_eq!(
            decode_mouse(ev(
                MouseEventKind::Down(HostButton::Left),
                coord,
                coord,
                KeyModifiers::NONE
            ))
            .at,
            Point { x: coord, y: coord },
            "coordinate {coord}"
        );
    }
}

#[test]
fn the_two_axes_are_carried_independently() {
    // A max column with a zero row must not swap or clamp: x and y are copied
    // separately.
    assert_eq!(
        decode_mouse(ev(MouseEventKind::Moved, u16::MAX, 0, KeyModifiers::NONE)).at,
        Point { x: u16::MAX, y: 0 }
    );
}
