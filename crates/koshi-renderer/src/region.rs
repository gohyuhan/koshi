//! What koshi's chrome rows draw from, and the compiled-in region solve that
//! places them. Each row input holds only shared references and copies.

use koshi_core::geometry::Size;
use koshi_core::key::KeySequence;
use koshi_core::lock::LockMode;
use koshi_layout::regions::{solve, Edge, RegionGeometry, RegionSolve};

use crate::snapshot::{KeymapHints, Reconnecting, TabMeta};

/// The compiled-in region geometry, in solve order: a one-row tabline on the
/// top edge, then a one-row statusline on the bottom edge.
const CORE_REGION_GEOMETRIES: [RegionGeometry; 2] = [
    RegionGeometry {
        edge: Edge::Top,
        extent: 1,
    },
    RegionGeometry {
        edge: Edge::Bottom,
        extent: 1,
    },
];

/// Solve the compiled-in tabline and statusline regions for `viewport`.
#[must_use]
pub fn core_region_solve(viewport: Size) -> RegionSolve {
    solve(viewport, &CORE_REGION_GEOMETRIES)
}

/// The statusline facts needed to paint it: the keybinding hints and the open
/// key sequence.
///
/// This value excludes colors. [`crate::statusline_hints::draw_statusline`]
/// takes the theme as its own argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StatuslineInputs<'a> {
    /// Every binding in the viewer's current mode, with the prefix labels, the
    /// removals, and the keymap-reverted marker.
    pub(crate) hints: &'a KeymapHints,
    /// The chords already pressed of an open key sequence. `None` when no
    /// sequence is open.
    pub(crate) pending: Option<&'a KeySequence>,
}

/// The tabline facts needed to solve its geometry and paint it.
///
/// This value excludes colors and pane data. A session named `work` with tabs
/// `shell` and `logs` carries those names, their active markers, and the mode
/// values, but no pane slot or terminal grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TablineInputs<'a> {
    /// The session display name shown in the left block.
    pub(crate) session_name: &'a str,
    /// The tab metadata shown between the left and right blocks.
    pub(crate) tabs: &'a [TabMeta],
    /// The viewing client's lock state shown in the mode tag.
    pub(crate) lock_mode: LockMode,
    /// Whether the viewing client is selecting with the mouse.
    pub(crate) mouse_select: bool,
    /// The reconnect state shown in the mode tag, if the viewer has no link.
    pub(crate) reconnecting: Option<Reconnecting>,
    /// The first tab index the viewer is peeking at, if one is set.
    pub(crate) tabline_offset: Option<usize>,
}

#[cfg(test)]
mod tests;
