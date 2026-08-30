//! What koshi's chrome rows draw from, and the compiled-in region solve that
//! places them. Each row input holds only shared references and copies.

use koshi_core::geometry::Size;
use koshi_core::key::KeySequence;
use koshi_core::lock::LockMode;
use koshi_layout::regions::{solve, Edge, RegionGeometry, RegionSolve};

use crate::snapshot::{KeymapHints, Reconnecting, TabMeta};
use crate::theme::Theme;

/// The compiled-in region geometry, in solve order: a one-row navigator on the
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

/// Solve the compiled-in navigator and statusline regions for `viewport`.
#[must_use]
pub fn core_region_solve(viewport: Size) -> RegionSolve {
    solve(viewport, &CORE_REGION_GEOMETRIES)
}

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

/// The tab-row facts needed to solve its geometry.
///
/// This value excludes colors and pane data. A session named `work` with tabs
/// `shell` and `logs` carries those names, their active markers, and the mode
/// values, but no pane slot or terminal grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct NavigatorLayout<'a> {
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

/// Everything the tab row is painted from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NavigatorDto<'a> {
    /// The session display name shown in the left block.
    pub session_name: &'a str,
    /// The tab metadata shown between the left and right blocks.
    pub tabs: &'a [TabMeta],
    /// The viewing client's lock state shown in the mode tag.
    pub lock_mode: LockMode,
    /// Whether the viewing client is selecting with the mouse.
    pub mouse_select: bool,
    /// The reconnect state shown in the mode tag, if the viewer has no link.
    pub reconnecting: Option<Reconnecting>,
    /// The first tab index the viewer is peeking at, if one is set.
    pub tabline_offset: Option<usize>,
    /// The colors the row is painted in.
    pub theme: &'a Theme,
}

impl<'a> NavigatorDto<'a> {
    /// Select the tab-row facts that the renderer and hit-test share.
    #[must_use]
    pub(crate) fn inputs(&self) -> NavigatorLayout<'a> {
        NavigatorLayout {
            session_name: self.session_name,
            tabs: self.tabs,
            lock_mode: self.lock_mode,
            mouse_select: self.mouse_select,
            reconnecting: self.reconnecting,
            tabline_offset: self.tabline_offset,
        }
    }
}

#[cfg(test)]
mod tests;
