//! Crossterm mouse boundary: one host mouse event becomes one canonical
//! [`MouseInput`].
//!
//! This is the mouse peer of [`decode_key`](crate::keyboard::decode_key). It is
//! a pure mapping — every host event turns into exactly one koshi event, so
//! unlike a key press (where a release yields nothing) there is no `None` case:
//! a mouse release and a bare motion are both real events koshi keeps.
//!
//! The coordinate that comes out is a raw client cell. Which pane, border, or
//! bar it lands on is decided later by a hit-test against the client's render
//! layout, not here.

use crossterm::event::{KeyModifiers, MouseButton as HostButton, MouseEvent, MouseEventKind};
use koshi_core::geometry::Point;
use koshi_core::key::ModFlags;
use koshi_core::mouse::{MouseButton, MouseInput, MouseKind, ScrollDirection};

/// Decode one host mouse event into its canonical [`MouseInput`].
///
/// A left press at column 10, row 3 becomes
/// `MouseInput { kind: Press(Left), at: Point { x: 10, y: 3 }, mods: NONE }`;
/// a wheel tick towards the user becomes `Scroll(Down)` at the pointer cell.
#[must_use]
pub fn decode_mouse(event: MouseEvent) -> MouseInput {
    MouseInput {
        kind: decode_kind(event.kind),
        at: Point {
            x: event.column,
            y: event.row,
        },
        mods: decode_mods(event.modifiers),
    }
}

/// Map the host's event kind onto koshi's. Down/Up/Drag carry the button
/// through [`button`]; the four scroll kinds carry a direction; a buttonless
/// move is [`MouseKind::Motion`].
fn decode_kind(kind: MouseEventKind) -> MouseKind {
    match kind {
        MouseEventKind::Down(b) => MouseKind::Press(button(b)),
        MouseEventKind::Up(b) => MouseKind::Release(button(b)),
        MouseEventKind::Drag(b) => MouseKind::Drag(button(b)),
        MouseEventKind::Moved => MouseKind::Motion,
        MouseEventKind::ScrollUp => MouseKind::Scroll(ScrollDirection::Up),
        MouseEventKind::ScrollDown => MouseKind::Scroll(ScrollDirection::Down),
        MouseEventKind::ScrollLeft => MouseKind::Scroll(ScrollDirection::Left),
        MouseEventKind::ScrollRight => MouseKind::Scroll(ScrollDirection::Right),
    }
}

/// Map the host button onto koshi's.
fn button(b: HostButton) -> MouseButton {
    match b {
        HostButton::Left => MouseButton::Left,
        HostButton::Middle => MouseButton::Middle,
        HostButton::Right => MouseButton::Right,
    }
}

/// The modifiers held during the event. Unlike a key press, a mouse event folds
/// nothing into Shift, so all four modifiers pass straight through: the
/// keyboard decoder supplies Ctrl/Alt/Super and Shift is added here.
fn decode_mods(modifiers: KeyModifiers) -> ModFlags {
    let mods = crate::keyboard::decode_mods(modifiers);
    if modifiers.contains(KeyModifiers::SHIFT) {
        mods.union(ModFlags::SHIFT)
    } else {
        mods
    }
}

#[cfg(test)]
mod tests;
