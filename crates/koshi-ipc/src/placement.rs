//! Pane placement previews: the layouts a viewer draws one placement preview
//! from.
//!
//! A preview rides the painted update of the client that names a placement
//! target ([`painted_update`](crate::painted_update)). It carries the source
//! pane's tab, or no tab when that pane floats, the destination tab's tree and
//! solved slots, and the tab sizes the session checks a placement at. The
//! panes' content travels in the frame's own panes, and the preview changes no
//! session state.

use std::collections::HashSet;

use koshi_core::geometry::{Rect, SplitDirection};
use koshi_core::ids::{PaneId, TabId};
use koshi_layout::mode::LayoutMode;
use koshi_layout::placement::{PlacementTabSizes, SwapTabSizes};
use koshi_layout::size::SizeConstraint;
use koshi_layout::solver::PaneSizing;
use koshi_layout::tree::LayoutNode;
use serde::{Deserialize, Serialize};

use crate::frame::FrameTab;

/// One placement target a viewer previews: the pane it places and the tab it
/// may place the pane in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PanePlacementPreviewTarget {
    /// The pane being placed.
    pub source_pane_id: PaneId,
    /// The tab the pane may move to.
    pub destination_tab_id: TabId,
}

/// The pane placement preview of one placement target: a source pane and the
/// destination tab it may move to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PanePlacementPreview {
    /// The layouts the target's source pane and destination tab have now.
    Available(Box<PanePlacementSnapshot>),
    /// The session holds the target's source pane in no tab and among no
    /// floating panes, or holds no tab with its destination tab id.
    Unavailable {
        /// The source pane of the target.
        source_pane_id: PaneId,
        /// The destination tab of the target.
        destination_tab_id: TabId,
    },
}

/// The layouts one client's pane placement preview is built from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PanePlacementSnapshot {
    /// The pane being placed.
    pub source_pane_id: PaneId,
    /// Where `source_pane_id` sits: the tab whose layout holds it, or
    /// [`PanePlacementSource::Floating`].
    pub source: PanePlacementSource,
    /// The tab the pane may move to.
    pub destination_tab_id: TabId,
    /// The destination tab's tree and solved layout. `None` exactly when the
    /// destination is the tab of a [`PanePlacementSource::Tiled`] source.
    pub destination_tab_snapshot: Option<PanePlacementTabSnapshot>,
    /// The shared sizing values used by the layout solver.
    pub pane_sizing: PanePlacementSizing,
    /// The tab sizes the session checks and solves a split or a stack at, and
    /// any placement within the source pane's own tab or of a floating source
    /// pane.
    pub insertion_tab_sizes: PlacementTabSizes,
    /// The tab sizes of a swap with each destination pane, in the destination
    /// tree's leaf order, when the source pane is tiled in another tab than
    /// the destination. Empty otherwise.
    pub swap_tab_sizes: Vec<SwapTabSizes>,
}

/// Where the source pane of a placement preview sits before the placement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PanePlacementSource {
    /// A leaf of this tab's layout: the source tab's tree and solved layout.
    Tiled(PanePlacementTabSnapshot),
    /// A floating pane.
    Floating,
}

/// The source or destination tab in a placement preview.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PanePlacementTabSnapshot {
    /// The tab's unsolved layout tree, each stack at its shared active member.
    /// The client edits it for local placement previews.
    pub layout_tree: LayoutNode,
    /// The client's focus history in the tab, newest first, with
    /// only panes of `layout_tree`. Each stack of `tab_snapshot` shows the
    /// member this history gives ([`LayoutNode::build_shown_layout_tree`]).
    pub focus_history: Vec<PaneId>,
    /// The tab's solved layout for the client whose frames carry the preview,
    /// tiled.
    pub tab_snapshot: FrameTab,
}

/// The shared layout sizing inputs used for a placement preview.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PanePlacementSizing {
    /// The minimum pane size used by the solver.
    pub minimum_size: koshi_core::geometry::Size,
    /// The blank-cell gap between adjacent split panes.
    pub gap_cell_count: u16,
}

/// The same minimum pane size and gap, field for field.
impl From<PaneSizing> for PanePlacementSizing {
    fn from(pane_sizing: PaneSizing) -> Self {
        PanePlacementSizing {
            minimum_size: pane_sizing.minimum_size,
            gap_cell_count: pane_sizing.gap_cell_count,
        }
    }
}

/// The same minimum pane size and gap, field for field.
impl From<PanePlacementSizing> for PaneSizing {
    fn from(pane_placement_sizing: PanePlacementSizing) -> Self {
        PaneSizing {
            minimum_size: pane_placement_sizing.minimum_size,
            gap_cell_count: pane_placement_sizing.gap_cell_count,
        }
    }
}

/// Why a decoded placement preview is not safe to draw from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanePlacementSnapshotValidationError {
    /// The destination tab id disagrees with its record, or the destination
    /// record is present for the source tab or absent for any other tab.
    TabIdentityMismatch,
    /// The source pane is not in the source tab.
    SourcePaneMissing,
    /// The same pane id appears more than once in the preview.
    DuplicatePaneId,
    /// The layout tree is not a canonical tree for the pane slots, a tab is
    /// not tiled or has another gap than the preview's sizing, the focus
    /// history repeats a pane or names a pane the tree does not hold, or a
    /// stack header disagrees with the tab.
    LayoutMismatch,
    /// The swap tab sizes do not name each destination pane once, in leaf
    /// order, for a tiled source in another tab, or are not empty for any
    /// other preview.
    TabSizesMismatch,
    /// A pane slot has inconsistent geometry or visibility flags.
    PaneSlotMismatch,
}

impl PanePlacementPreview {
    /// The target this preview shows.
    #[must_use]
    pub fn get_placement_target(&self) -> PanePlacementPreviewTarget {
        match self {
            PanePlacementPreview::Available(placement_snapshot) => PanePlacementPreviewTarget {
                source_pane_id: placement_snapshot.source_pane_id,
                destination_tab_id: placement_snapshot.destination_tab_id,
            },
            PanePlacementPreview::Unavailable {
                source_pane_id,
                destination_tab_id,
            } => PanePlacementPreviewTarget {
                source_pane_id: *source_pane_id,
                destination_tab_id: *destination_tab_id,
            },
        }
    }
}

impl PanePlacementSnapshot {
    /// The tab snapshot whose layout holds the source pane, or `None` when the
    /// source pane floats.
    #[must_use]
    pub fn find_source_tab_snapshot(&self) -> Option<&PanePlacementTabSnapshot> {
        match &self.source {
            PanePlacementSource::Tiled(source_tab_snapshot) => Some(source_tab_snapshot),
            PanePlacementSource::Floating => None,
        }
    }

    /// Validate the relationships a client relies on to draw and edit the
    /// preview.
    ///
    /// # Errors
    ///
    /// - [`PanePlacementSnapshotValidationError::TabIdentityMismatch`] when
    ///   the destination snapshot names another tab than
    ///   `destination_tab_id`, or is present for the source tab or absent for
    ///   any other tab. A floating source counts as no tab.
    /// - [`PanePlacementSnapshotValidationError::SourcePaneMissing`] when the
    ///   source tab's slots do not hold `source_pane_id`.
    /// - [`PanePlacementSnapshotValidationError::DuplicatePaneId`] when a pane
    ///   appears in both tabs, twice in one tab, or as a floating source pane
    ///   in the destination tab.
    /// - [`PanePlacementSnapshotValidationError::LayoutMismatch`] for a tab
    ///   whose tree, focus history or stack headers disagree with its slots,
    ///   and [`PanePlacementSnapshotValidationError::PaneSlotMismatch`] for a
    ///   slot whose geometry or flags disagree.
    /// - [`PanePlacementSnapshotValidationError::TabSizesMismatch`] when the
    ///   swap tab sizes do not name each destination pane once, in leaf order,
    ///   for a tiled source in another tab, or are not empty for any other
    ///   preview.
    pub fn validate(&self) -> Result<(), PanePlacementSnapshotValidationError> {
        let source_tab_id = self
            .find_source_tab_snapshot()
            .map(|source_tab_snapshot| source_tab_snapshot.tab_snapshot.tab_id);
        if self
            .destination_tab_snapshot
            .as_ref()
            .is_some_and(|tab_snapshot| tab_snapshot.tab_snapshot.tab_id != self.destination_tab_id)
            || self.destination_tab_snapshot.is_none()
                != (source_tab_id == Some(self.destination_tab_id))
        {
            return Err(PanePlacementSnapshotValidationError::TabIdentityMismatch);
        }
        let mut preview_pane_ids = HashSet::new();
        if let Some(source_tab_snapshot) = self.find_source_tab_snapshot() {
            validate_tab_snapshot(
                source_tab_snapshot,
                self.pane_sizing.gap_cell_count,
                &mut preview_pane_ids,
            )?;
            if !source_tab_snapshot
                .tab_snapshot
                .pane_slots
                .iter()
                .any(|pane_slot| pane_slot.pane_id == self.source_pane_id)
            {
                return Err(PanePlacementSnapshotValidationError::SourcePaneMissing);
            }
        }
        if let Some(destination_tab_snapshot) = &self.destination_tab_snapshot {
            validate_tab_snapshot(
                destination_tab_snapshot,
                self.pane_sizing.gap_cell_count,
                &mut preview_pane_ids,
            )?;
        }
        if source_tab_id.is_none() && !preview_pane_ids.insert(self.source_pane_id) {
            return Err(PanePlacementSnapshotValidationError::DuplicatePaneId);
        }
        let swap_target_pane_ids: Vec<PaneId> = self
            .swap_tab_sizes
            .iter()
            .map(|swap_tab_sizes| swap_tab_sizes.target_pane_id)
            .collect();
        let expected_swap_target_pane_ids = match (&self.source, &self.destination_tab_snapshot) {
            (PanePlacementSource::Tiled(_), Some(destination_tab_snapshot)) => {
                destination_tab_snapshot
                    .tab_snapshot
                    .pane_slots
                    .iter()
                    .map(|pane_slot| pane_slot.pane_id)
                    .collect()
            }
            _ => Vec::new(),
        };
        if swap_target_pane_ids != expected_swap_target_pane_ids {
            return Err(PanePlacementSnapshotValidationError::TabSizesMismatch);
        }
        Ok(())
    }
}

/// Check one tab of a preview: it is tiled with `expected_gap_cell_count`, its
/// tree is canonical and lists the slots' panes in slot order, its focus
/// history names only those panes and each once, and its slots and stack
/// headers sit inside the tab. Adds the tab's panes to `preview_pane_ids`.
fn validate_tab_snapshot(
    placement_tab_snapshot: &PanePlacementTabSnapshot,
    expected_gap_cell_count: u16,
    preview_pane_ids: &mut HashSet<PaneId>,
) -> Result<(), PanePlacementSnapshotValidationError> {
    let tab_snapshot = &placement_tab_snapshot.tab_snapshot;
    if tab_snapshot.layout_mode != LayoutMode::Tiled
        || tab_snapshot.gap_cell_count != expected_gap_cell_count
    {
        return Err(PanePlacementSnapshotValidationError::LayoutMismatch);
    }
    let layout_pane_ids = collect_layout_pane_ids(&placement_tab_snapshot.layout_tree)?;
    let slot_pane_ids: Vec<PaneId> = tab_snapshot
        .pane_slots
        .iter()
        .map(|pane_slot| pane_slot.pane_id)
        .collect();
    for pane_id in &slot_pane_ids {
        if !preview_pane_ids.insert(*pane_id) {
            return Err(PanePlacementSnapshotValidationError::DuplicatePaneId);
        }
    }
    if layout_pane_ids != slot_pane_ids {
        return Err(PanePlacementSnapshotValidationError::LayoutMismatch);
    }
    let layout_pane_id_set: HashSet<PaneId> = layout_pane_ids.iter().copied().collect();
    let mut focus_history_pane_ids = HashSet::new();
    for pane_id in &placement_tab_snapshot.focus_history {
        if !layout_pane_id_set.contains(pane_id) || !focus_history_pane_ids.insert(*pane_id) {
            return Err(PanePlacementSnapshotValidationError::LayoutMismatch);
        }
    }

    let tab_rect = Rect::from_size_at_origin(tab_snapshot.tab_size);
    for pane_slot in &tab_snapshot.pane_slots {
        if !pane_slot.outer_rect.is_inside_rect(tab_rect)
            || pane_slot.is_visible != pane_slot.content_rect.is_some()
            || pane_slot.content_rect.is_some_and(|content_rect| {
                content_rect.is_empty() || !content_rect.is_inside_rect(pane_slot.outer_rect)
            })
            || (pane_slot.is_suppressed && pane_slot.is_visible)
        {
            return Err(PanePlacementSnapshotValidationError::PaneSlotMismatch);
        }
    }
    let is_every_pane_suppressed = !tab_snapshot.pane_slots.is_empty()
        && tab_snapshot
            .pane_slots
            .iter()
            .all(|pane_slot| pane_slot.is_suppressed);
    if tab_snapshot.is_every_pane_suppressed != is_every_pane_suppressed {
        return Err(PanePlacementSnapshotValidationError::PaneSlotMismatch);
    }
    validate_stack_headers(tab_snapshot, &layout_pane_id_set)
}

/// The panes of `layout_tree`, in leaf order.
///
/// # Errors
///
/// [`PanePlacementSnapshotValidationError::LayoutMismatch`] when a split holds
/// fewer than two children, another number of weights than children, an
/// invalid weight, or an active child index that is out of range for a stack
/// or not `0` for another split.
fn collect_layout_pane_ids(
    layout_tree: &LayoutNode,
) -> Result<Vec<PaneId>, PanePlacementSnapshotValidationError> {
    let mut layout_nodes = vec![layout_tree];
    let mut layout_pane_ids = Vec::new();
    while let Some(layout_node) = layout_nodes.pop() {
        match layout_node {
            LayoutNode::Pane(pane_id) => layout_pane_ids.push(*pane_id),
            LayoutNode::Split(split_node) => {
                if split_node.children.len() < 2
                    || split_node.weights.len() != split_node.children.len()
                    || (split_node.direction == SplitDirection::Stacked
                        && split_node.active_child_index >= split_node.children.len())
                    || (split_node.direction != SplitDirection::Stacked
                        && split_node.active_child_index != 0)
                    || split_node
                        .weights
                        .iter()
                        .any(|size_weight| !is_valid_size_weight(size_weight))
                {
                    return Err(PanePlacementSnapshotValidationError::LayoutMismatch);
                }
                layout_nodes.extend(split_node.children.iter().rev());
            }
        }
    }
    Ok(layout_pane_ids)
}

/// Whether `size_weight` holds a positive weight, a percent from 1 to 100, or
/// a positive cell count, and positive optional bounds.
fn is_valid_size_weight(size_weight: &koshi_layout::size::SizeWeight) -> bool {
    let is_valid_primary_constraint = match size_weight.primary_constraint {
        SizeConstraint::Flex(flex_weight) => flex_weight > 0,
        SizeConstraint::Percent(percent_value) => (1..=100).contains(&percent_value),
        SizeConstraint::Fixed(cell_count)
        | SizeConstraint::Minimum(cell_count)
        | SizeConstraint::Preferred(cell_count) => cell_count > 0,
    };
    is_valid_primary_constraint
        && size_weight
            .minimum_cell_count
            .is_none_or(|cell_count| cell_count > 0)
        && size_weight
            .preferred_cell_count
            .is_none_or(|cell_count| cell_count > 0)
}

/// Check that each stack header names a pane of `layout_pane_ids` once, counts
/// at least two members with its index among them, and sits inside the tab.
fn validate_stack_headers(
    tab_snapshot: &FrameTab,
    layout_pane_ids: &HashSet<PaneId>,
) -> Result<(), PanePlacementSnapshotValidationError> {
    let mut header_pane_ids = HashSet::new();
    let tab_rect = Rect::from_size_at_origin(tab_snapshot.tab_size);
    for stack_header in &tab_snapshot.stack_headers {
        if !layout_pane_ids.contains(&stack_header.pane_id)
            || !header_pane_ids.insert(stack_header.pane_id)
            || stack_header.member_count < 2
            || stack_header.member_index >= stack_header.member_count
            || !stack_header.header_rect.is_inside_rect(tab_rect)
        {
            return Err(PanePlacementSnapshotValidationError::LayoutMismatch);
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests;
