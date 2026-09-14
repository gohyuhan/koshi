//! Host mouse boundary: one host mouse event becomes one canonical
//! [`MouseInput`].
//!
//! This is the mouse peer of [`decode_key`](crate::keyboard::decode_key). Every
//! host event turns into exactly one koshi event: a mouse release and a bare
//! motion are both kept.
//!
//! The coordinate that comes out is a raw client cell. A hit-test against the
//! client's render layout, elsewhere, decides which pane, border, or bar it
//! lands on.

use crate::host::{Modifiers, MouseButton as HostButton, MouseEvent, MouseEventKind};
use koshi_core::geometry::Point;
use koshi_core::key::ModFlags;
use koshi_core::mouse::{MouseButton, MouseInput, MouseKind, ScrollDirection};

/// Decode one host mouse event into its canonical [`MouseInput`].
///
/// The host parser converts SGR coordinates to zero-based cells, so a left press at
/// protocol column 11, row 4 becomes
/// `MouseInput { mouse_kind: Press(Left), position: Point { column: 10, row: 3 }, modifier_flags: NONE }`;
/// a wheel tick towards the user becomes `Scroll(Down)` at the pointer cell.
#[must_use]
pub fn decode_mouse(mouse_event: MouseEvent) -> MouseInput {
    MouseInput {
        mouse_kind: decode_mouse_kind(mouse_event.mouse_event_kind),
        position: Point {
            column: mouse_event.column,
            row: mouse_event.row,
        },
        modifier_flags: decode_modifiers(mouse_event.modifiers),
    }
}

/// Map the host's event kind onto koshi's. Down/Up/Drag carry the button
/// through [`decode_button`]; the four scroll kinds carry a direction; a buttonless
/// move is [`MouseKind::Motion`].
fn decode_mouse_kind(mouse_event_kind: MouseEventKind) -> MouseKind {
    match mouse_event_kind {
        MouseEventKind::Down(button) => MouseKind::Press(decode_button(button)),
        MouseEventKind::Up(button) => MouseKind::Release(decode_button(button)),
        MouseEventKind::Drag(button) => MouseKind::Drag(decode_button(button)),
        MouseEventKind::Moved => MouseKind::Motion,
        MouseEventKind::ScrollUp => MouseKind::Scroll(ScrollDirection::Up),
        MouseEventKind::ScrollDown => MouseKind::Scroll(ScrollDirection::Down),
        MouseEventKind::ScrollLeft => MouseKind::Scroll(ScrollDirection::Left),
        MouseEventKind::ScrollRight => MouseKind::Scroll(ScrollDirection::Right),
    }
}

/// Map the host button onto koshi's.
fn decode_button(button: HostButton) -> MouseButton {
    match button {
        HostButton::Left => MouseButton::Left,
        HostButton::Middle => MouseButton::Middle,
        HostButton::Right => MouseButton::Right,
    }
}

/// The modifiers held during the event: Control, Alt and Super from
/// [`crate::keyboard::decode_modifiers`], plus Shift. Meta counts as Super; Hyper
/// is dropped.
fn decode_modifiers(host_modifiers: Modifiers) -> ModFlags {
    let modifier_flags = crate::keyboard::decode_modifiers(host_modifiers);
    if host_modifiers.has_all_modifiers(Modifiers::SHIFT) {
        modifier_flags.union(ModFlags::SHIFT)
    } else {
        modifier_flags
    }
}

#[cfg(test)]
mod tests;
