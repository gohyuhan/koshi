//! What koshi's chrome rows draw from, and the compiled-in region solve that
//! places them. Each row input holds only shared references and copies.

use koshi_core::geometry::Size;
use koshi_core::key::KeySequence;
use koshi_core::lock::LockMode;
use koshi_layout::regions::{solve_region_rects, Edge, RegionGeometry, SolvedRegions};

use crate::snapshot::{KeymapHints, NotificationButton, PeerSnapshot, Reconnecting, TabMetadata};

/// The compiled-in region geometry, in solve order: a one-row tabline on the
/// top edge, then a one-row statusline on the bottom edge.
const CORE_REGION_GEOMETRIES: [RegionGeometry; 2] = [
    RegionGeometry {
        edge: Edge::Top,
        extent_cell_count: 1,
    },
    RegionGeometry {
        edge: Edge::Bottom,
        extent_cell_count: 1,
    },
];

/// Solve the compiled-in tabline and statusline regions for `viewport_size`.
#[must_use]
pub fn solve_core_regions(viewport_size: Size) -> SolvedRegions {
    solve_region_rects(viewport_size, &CORE_REGION_GEOMETRIES)
}

/// The statusline facts needed to paint it: keybinding hints, the open key
/// sequence and the count of panes that wait for a person.
///
/// This value excludes colors. [`crate::statusline_hints::draw_statusline`]
/// takes the theme as its own argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StatuslineInputs<'a> {
    /// Every binding in the viewer's current mode, with the prefix labels and
    /// the removals.
    pub(crate) keymap_hints: &'a KeymapHints,
    /// The chords already pressed of an open key sequence. `None` when no
    /// sequence is open.
    pub(crate) pending_key_sequence: Option<&'a KeySequence>,
    /// How many panes of the session wait for a person, the panes of its
    /// floating panes included.
    pub(crate) waiting_pane_count: usize,
}

/// The tabline facts needed to solve its geometry and paint it.
///
/// This value excludes colors and pane data. A session named `work` with tabs
/// `shell` and `logs` carries those names, their active markers, the mode
/// values, the tray button state, the notification button state and the
/// indexes of the attached clients, but no pane slot or terminal grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TablineInputs<'a> {
    /// The session display name shown in the left block.
    pub(crate) session_name: &'a str,
    /// The tab metadata shown between the left and right blocks.
    pub(crate) tabs_metadata: &'a [TabMetadata],
    /// The viewing client's lock state shown in the mode tag.
    pub(crate) lock_mode: LockMode,
    /// Whether the viewing client is selecting with the mouse.
    pub(crate) is_mouse_selection_enabled: bool,
    /// The reconnect state shown in the mode tag, if the viewer has no link.
    pub(crate) reconnecting: Option<Reconnecting>,
    /// The first tab index the viewer is peeking at, if one is set.
    pub(crate) tabline_offset: Option<usize>,
    /// The notification button the tabline draws left of the tray button.
    pub(crate) notification_button: NotificationButton,
    /// The viewing client's own index, which its chip shows as
    /// `viewer_color_index + 1`.
    pub(crate) viewer_color_index: u32,
    /// Every other client attached to the session, in `color_index` order.
    /// Empty while the viewing client is the only one, and then the tabline
    /// draws no client chip.
    pub(crate) peers: &'a [PeerSnapshot],
}

#[cfg(test)]
mod tests;
