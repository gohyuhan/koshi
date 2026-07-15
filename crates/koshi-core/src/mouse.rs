//! Mouse vocabulary: the button, scroll direction, and decoded-event types the
//! rest of koshi reasons about — koshi's own terms, not the host library's.
//!
//! [`MouseButton`] and [`ScrollDirection`] are the primitive types; the bus
//! events in [`crate::event`] (`MousePressed`, `MouseScrolled`, …) compose their
//! payloads from them, and so does [`MouseInput`]. One button type and one
//! scroll type serve the whole crate.
//!
//! A [`MouseInput`] is the mouse peer of a [`KeyChord`](crate::key::KeyChord):
//! the boundary that decodes a host event produces one of these and nothing
//! host-specific escapes it. Its coordinate is a [`Point`] — a cell in the
//! client's own screen, still raw. Nothing here says which pane, border, or bar
//! that cell falls in; that hit-test happens later, against the client's render
//! layout. The type carries no client identity: which client the press came
//! from is the caller's to attach when it hands the event to the hit-test, the
//! same way a decoded key chord travels without one.

use crate::geometry::Point;
use crate::key::ModFlags;
use serde::{Deserialize, Serialize};

/// A mouse button.
///
/// Some terminals cannot tell koshi which button a release or drag used and
/// report [`Left`](MouseButton::Left) as a stand-in; the value is whatever the
/// host claimed, carried faithfully.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MouseButton {
    /// The left button.
    Left,
    /// The middle button (wheel click).
    Middle,
    /// The right button.
    Right,
}

/// The direction a wheel or trackpad scrolled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScrollDirection {
    /// Away from the user.
    Up,
    /// Towards the user.
    Down,
    /// Leftwards (mostly a trackpad).
    Left,
    /// Rightwards (mostly a trackpad).
    Right,
}

/// What the mouse did, with the button or scroll direction it did it with.
///
/// [`Motion`](MouseKind::Motion) is the pointer moving with no button held — a
/// real event a program in application-mouse mode can ask to receive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseKind {
    /// A button went down. `Press(Left)` is a left click starting.
    Press(MouseButton),
    /// A button came up.
    Release(MouseButton),
    /// The pointer moved with a button held.
    Drag(MouseButton),
    /// The wheel or trackpad scrolled.
    Scroll(ScrollDirection),
    /// The pointer moved with no button held.
    Motion,
}

/// One decoded mouse event: what happened, at which client cell, with which
/// modifiers held.
///
/// A left click at column 10, row 3 with nothing held is
/// `MouseInput { kind: Press(Left), at: Point { x: 10, y: 3 }, mods: NONE }`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseInput {
    /// What the mouse did.
    pub kind: MouseKind,
    /// The client cell the event landed on — raw, not yet hit-tested.
    pub at: Point,
    /// The modifier keys held during the event.
    pub mods: ModFlags,
}
