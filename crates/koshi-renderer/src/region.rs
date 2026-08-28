//! What koshi's chrome rows draw from, and the compiled-in region solve that
//! places them. Each row input holds only shared references and copies.

use koshi_core::geometry::Size;
use koshi_core::key::KeySequence;
use koshi_layout::regions::{solve, Edge, RegionGeometry, RegionSolve};

use crate::snapshot::{FrameLayout, KeymapHints};
use crate::theme::Theme;

/// The compiled-in navigator and statusline inputs.
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

/// Everything the tab row is painted from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NavigatorDto<'a> {
    /// The session being viewed, the viewing client, and the viewer's own
    /// chrome state, including the committed region solve when one exists.
    pub frame: FrameLayout<'a>,
    /// The colors the row is painted in.
    pub theme: &'a Theme,
}

#[cfg(test)]
mod tests;
