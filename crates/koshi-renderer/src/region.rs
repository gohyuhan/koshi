//! What koshi's two chrome rows draw from, borrowed out of the frame being
//! painted: the keybinding hint row's data, and the tab row's data. Each type
//! holds only shared references and copies. Building one clones nothing.

use koshi_core::key::KeySequence;

use crate::snapshot::{FrameLayout, KeymapHints};
use crate::theme::Theme;

/// Everything the keybinding hint row is painted from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatuslineDto<'a> {
    /// Every binding in the viewer's current mode, with the prefix labels, the
    /// removals, and the keymap-reverted marker.
    pub hints: &'a KeymapHints,
    /// The colors the row is painted in.
    pub theme: &'a Theme,
    /// The chords already pressed of an open key sequence. `None` when no
    /// sequence is open.
    pub pending: Option<&'a KeySequence>,
}

/// Everything the tab row is painted from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NavigatorDto<'a> {
    /// The session being viewed, the viewing client, and the viewer's own
    /// chrome state.
    pub frame: FrameLayout<'a>,
    /// The colors the row is painted in.
    pub theme: &'a Theme,
}

#[cfg(test)]
mod tests;
