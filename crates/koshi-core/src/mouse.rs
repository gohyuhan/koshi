//! Mouse vocabulary: the button, scroll direction, decoded-event, answer, and
//! reporting-level types the rest of koshi reasons about.
//!
//! [`MouseButton`] and [`ScrollDirection`] are the primitive types; the bus
//! events in [`crate::event`] (`MousePressed`, `MouseScrolled`, …) compose their
//! payloads from them, and so does [`MouseInput`]. One button type and one
//! scroll type serve the whole crate.
//!
//! [`MouseTracking`] says which events the program in a pane asked to receive,
//! and [`is_mouse_kind_reported`] answers that question for one event. The viewer reads them
//! off a painted frame to decide where a mouse event goes; the session reads
//! them off live state to decide what to write.
//!
//! A [`MouseInput`] is the mouse peer of a [`KeyChord`](crate::key::KeyChord):
//! the boundary that decodes a host event produces one. Its coordinate is a
//! [`Point`], a raw cell in the client's own screen; which pane, border, or
//! bar that cell falls in is hit-tested against the client's render layout.
//! The type carries no client identity; the caller attaches that when it
//! hands the event to the hit-test.
//!
//! [`MouseAnswer`] runs the other way — the session says what an action it
//! carried out did, and the client folds that into its gesture state.

use crate::geometry::{Direction, Point};
use crate::ids::PaneId;
use crate::key::ModFlags;
use serde::{Deserialize, Serialize};

/// A mouse button.
///
/// The value is whatever the host reported. A terminal that cannot say which
/// button a release or drag used reports [`Left`](MouseButton::Left).
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
/// `MouseInput { mouse_kind: Press(Left), position: Point { column: 10, row: 3 }, modifier_flags: NONE }`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MouseInput {
    /// What the mouse did.
    pub mouse_kind: MouseKind,
    /// The client cell the event landed on — raw, not yet hit-tested.
    pub position: Point,
    /// The modifier keys held during the event.
    pub modifier_flags: ModFlags,
}

/// What the session reports back about a mouse action it carried out.
///
/// An action that has nothing to report produces no `MouseAnswer`: there is no
/// empty variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MouseAnswer {
    /// Where the `pane_id` view landed after a scroll: `top_row_number` is the
    /// line now on its top row, or `None` for a pane with no terminal. Consumed by
    /// `Client::note_scroll_applied`.
    Scrolled {
        /// The pane whose view the scroll moved.
        pane_id: PaneId,
        /// The line the view now shows on its top row.
        top_row_number: Option<u64>,
    },
    /// How many cells of a requested border move the session accepted, which is
    /// fewer than asked for when the border hit a wall. Consumed by
    /// `Client::note_resize_applied`.
    ///
    /// `pane_id`, `border_side` and `resize_step` repeat the move this answers,
    /// so a round carrying several border moves is read back move by move.
    Resized {
        /// The pane whose border the move was asked for.
        pane_id: PaneId,
        /// Which of the pane's borders the move was asked for.
        border_side: Direction,
        /// The direction the move was asked in: `1` grows the pane, `-1`
        /// shrinks it.
        resize_step: i16,
        /// The number of cells the border actually moved.
        applied_cell_count: u16,
    },
}

/// Which mouse events the running app has asked to be reported, set via the DEC
/// private modes `?9`/`?1000`/`?1002`/`?1003`. The levels form a ladder (each
/// reports strictly more than the one above); an app enables exactly one, and
/// the last enabling sequence wins. Independent of how a report is encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
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
/// wheel tick reports from `Normal` up; `X10` reports only presses.
///
/// `is_mouse_kind_reported(MouseTracking::Normal, MouseKind::Scroll(ScrollDirection::Up))` is
/// `true`; `is_mouse_kind_reported(MouseTracking::X10, MouseKind::Scroll(ScrollDirection::Up))`
/// is `false`.
#[must_use]
pub fn is_mouse_kind_reported(mouse_tracking: MouseTracking, mouse_kind: MouseKind) -> bool {
    match mouse_kind {
        MouseKind::Press(_) => mouse_tracking != MouseTracking::Off,
        MouseKind::Release(_) | MouseKind::Scroll(_) => matches!(
            mouse_tracking,
            MouseTracking::Normal | MouseTracking::ButtonMotion | MouseTracking::AnyMotion
        ),
        MouseKind::Drag(_) => matches!(
            mouse_tracking,
            MouseTracking::ButtonMotion | MouseTracking::AnyMotion
        ),
        MouseKind::Motion => mouse_tracking == MouseTracking::AnyMotion,
    }
}

#[cfg(test)]
mod tests;
