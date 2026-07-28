//! Mouse vocabulary: the button, scroll direction, decoded-event, and
//! reporting-level types the rest of koshi reasons about — koshi's own terms,
//! not the host library's.
//!
//! [`MouseButton`] and [`ScrollDirection`] are the primitive types; the bus
//! events in [`crate::event`] (`MousePressed`, `MouseScrolled`, …) compose their
//! payloads from them, and so does [`MouseInput`]. One button type and one
//! scroll type serve the whole crate.
//!
//! [`MouseTracking`] says which events the program in a pane asked to receive,
//! and [`reports`] answers that question for one event. The viewer reads them
//! off a painted frame to decide where a mouse event goes; the session reads
//! them off live state to decide what to write.
//!
//! A [`MouseInput`] is the mouse peer of a [`KeyChord`](crate::key::KeyChord):
//! the boundary that decodes a host event produces one of these and nothing
//! host-specific escapes it. Its coordinate is a [`Point`] — a raw cell in the
//! client's own screen. Which pane, border, or bar that cell falls in is
//! hit-tested later against the client's render layout. The type carries no
//! client identity; the caller attaches that when it hands the event to the
//! hit-test.

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

/// Which mouse events the running app has asked to be reported, set via the DEC
/// private modes `?9`/`?1000`/`?1002`/`?1003`. The levels form a ladder (each
/// reports strictly more than the one above); an app enables exactly one, and
/// the last enabling sequence wins. Independent of how a report is encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MouseTracking {
    /// No mouse reporting (default).
    #[default]
    Off,
    /// `?9` X10 compatibility — button presses only, no releases.
    X10,
    /// `?1000` normal tracking — button presses and releases.
    Normal,
    /// `?1002` button-event tracking — presses, releases, and motion while a
    /// button is held (drag).
    ButtonMotion,
    /// `?1003` any-event tracking — all motion, whether or not a button is held.
    AnyMotion,
}

/// Whether a program at `tracking` is told about a `kind` of event. The ladder:
/// every level but `Off` reports a press, `Normal` and up add releases,
/// `ButtonMotion` and up add drags, only `AnyMotion` adds buttonless motion. A
/// wheel tick reports from `Normal` up — `X10` predates the wheel and reports
/// only presses.
///
/// `reports(MouseTracking::Normal, MouseKind::Scroll(ScrollDirection::Up))` is
/// `true`; `reports(MouseTracking::X10, MouseKind::Scroll(ScrollDirection::Up))`
/// is `false`.
#[must_use]
pub fn reports(tracking: MouseTracking, kind: MouseKind) -> bool {
    match kind {
        MouseKind::Press(_) => tracking != MouseTracking::Off,
        MouseKind::Release(_) | MouseKind::Scroll(_) => matches!(
            tracking,
            MouseTracking::Normal | MouseTracking::ButtonMotion | MouseTracking::AnyMotion
        ),
        MouseKind::Drag(_) => matches!(
            tracking,
            MouseTracking::ButtonMotion | MouseTracking::AnyMotion
        ),
        MouseKind::Motion => tracking == MouseTracking::AnyMotion,
    }
}

#[cfg(test)]
mod tests;
