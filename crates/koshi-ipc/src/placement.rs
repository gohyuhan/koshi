//! Bounded read-only placement previews.
//!
//! A placement preview carries the source and selected destination layout trees,
//! solved slots, visible pane cells, and image references. It carries no
//! scrollback metadata and does not change session state.

use std::collections::{hash_map::Entry, HashMap, HashSet};

use koshi_core::geometry::{PaneArea, Rect, Size, SplitDirection};
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};
use koshi_layout::mode::LayoutMode;
use koshi_layout::size::SizeConstraint;
use koshi_layout::tree::LayoutNode;
use koshi_pane::pane::state::PaneKind;
use serde::{Deserialize, Serialize};

use crate::frame::{
    compute_frame_image_byte_count, FrameImageAction, FrameImagePlacement, FrameSlot, FrameWindow,
};

/// The maximum number of layout leaves in one placement preview.
pub const MAX_PLACEMENT_SNAPSHOT_PANE_COUNT: usize = 256;

/// The maximum number of cells carried by one placement preview.
pub const MAX_PLACEMENT_SNAPSHOT_CELL_COUNT: u64 = 262_144;

/// The maximum number of image placements carried by one placement preview.
pub const MAX_PLACEMENT_SNAPSHOT_IMAGE_COUNT: usize = 4_096;

/// The maximum total RGBA bytes referenced by one placement preview.
pub const MAX_PLACEMENT_SNAPSHOT_IMAGE_BYTE_COUNT: u64 =
    crate::frame::MAX_FRAME_IMAGE_TRANSFER_BYTE_COUNT;

/// A read-only placement preview for one attached client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PanePlacementSnapshot {
    /// The session that produced the preview.
    pub session_id: SessionId,
    /// The source pane named by the request.
    pub source_pane_id: PaneId,
    /// The source tab that currently contains `source_pane_id`.
    pub source_tab_id: TabId,
    /// The destination tab named by the request.
    pub destination_tab_id: TabId,
    /// The session placement revision used to build the preview.
    pub session_placement_revision: u64,
    /// The requesting client's placement revision used to build the preview.
    pub client_placement_revision: u64,
    /// The source tab's read-only layout and pane content.
    pub source_tab_snapshot: PanePlacementTabSnapshot,
    /// The destination tab's read-only layout and visible pane content. `None`
    /// means the destination is the source tab and the source snapshot is used.
    pub destination_tab_snapshot: Option<PanePlacementTabSnapshot>,
    /// The requesting client's view inputs.
    pub client_snapshot: PanePlacementClientSnapshot,
    /// The shared sizing values used by the layout solver.
    pub pane_sizing: PanePlacementSizing,
}

const MAX_PLACEMENT_SNAPSHOT_LAYOUT_NODE_COUNT: usize = MAX_PLACEMENT_SNAPSHOT_PANE_COUNT * 2 - 1;
const MAX_PLACEMENT_SNAPSHOT_COMBINING_CHARACTER_COUNT: usize = 32;

struct PanePlacementValidationState {
    pane_ids: HashSet<PaneId>,
    pane_count: usize,
    cell_count: u64,
    image_count: usize,
    image_byte_count: u64,
    image_dimensions_by_content_id: HashMap<u64, (u32, u32)>,
    expected_gap_cell_count: u16,
}

impl PanePlacementSnapshot {
    /// Validate the bounds and relationships that protect a client from malformed previews.
    pub fn validate(&self) -> Result<(), PanePlacementSnapshotValidationError> {
        if self.source_tab_snapshot.tab_id != self.source_tab_id {
            return Err(PanePlacementSnapshotValidationError::TabIdentityMismatch);
        }
        if self
            .destination_tab_snapshot
            .as_ref()
            .is_some_and(|tab_snapshot| tab_snapshot.tab_id != self.destination_tab_id)
            || self.destination_tab_snapshot.is_none()
                != (self.source_tab_id == self.destination_tab_id)
        {
            return Err(PanePlacementSnapshotValidationError::TabIdentityMismatch);
        }

        let mut placement_validation = PanePlacementValidationState {
            pane_ids: HashSet::new(),
            pane_count: 0,
            cell_count: 0,
            image_count: 0,
            image_byte_count: 0,
            image_dimensions_by_content_id: HashMap::new(),
            expected_gap_cell_count: self.pane_sizing.gap_cell_count,
        };
        validate_tab_snapshot(
            &self.source_tab_snapshot,
            &mut placement_validation,
            (self.source_tab_id != self.destination_tab_id).then_some(self.source_pane_id),
        )?;
        if !self
            .source_tab_snapshot
            .pane_slots
            .iter()
            .any(|pane_slot| pane_slot.pane_id == self.source_pane_id)
        {
            return Err(PanePlacementSnapshotValidationError::SourcePaneMissing);
        }
        if let Some(destination_tab_snapshot) = &self.destination_tab_snapshot {
            validate_tab_snapshot(destination_tab_snapshot, &mut placement_validation, None)?;
        }
        if placement_validation.pane_count > MAX_PLACEMENT_SNAPSHOT_PANE_COUNT {
            return Err(PanePlacementSnapshotValidationError::PaneCountExceeded);
        }
        if placement_validation.cell_count > MAX_PLACEMENT_SNAPSHOT_CELL_COUNT {
            return Err(PanePlacementSnapshotValidationError::CellCountExceeded);
        }
        if placement_validation.image_count > MAX_PLACEMENT_SNAPSHOT_IMAGE_COUNT {
            return Err(PanePlacementSnapshotValidationError::ImageCountExceeded);
        }
        if placement_validation.image_byte_count > MAX_PLACEMENT_SNAPSHOT_IMAGE_BYTE_COUNT {
            return Err(PanePlacementSnapshotValidationError::ImageByteCountExceeded);
        }
        Ok(())
    }
}

/// The source or destination tab in a placement preview.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PanePlacementTabSnapshot {
    /// The tab's stable id.
    pub tab_id: TabId,
    /// The tab's display name.
    pub tab_name: String,
    /// The unsolved layout tree the client uses for local placement previews.
    pub layout_tree: LayoutNode,
    /// The solved pane slots for this client's preview view.
    pub pane_slots: Vec<FrameSlot>,
    /// The shared cell size used by these pane slots.
    pub effective_cell_size: Size,
    /// Header strips for collapsed stack members.
    pub stack_headers: Vec<koshi_layout::solver::StackHeader>,
    /// The layout mode this client retains for the tab.
    pub layout_mode: LayoutMode,
    /// Whether all panes are suppressed because the tab has no room.
    pub is_every_pane_suppressed: bool,
    /// Blank cells between adjacent split panes.
    pub gap_cell_count: u16,
    /// Visible cells and image references, matched to `pane_slots` by pane id.
    /// The source pane may retain cells while its source slot is suppressed so
    /// a cross-tab preview can display that pane after transfer.
    pub pane_snapshots: Vec<PanePlacementPaneSnapshot>,
}

/// One pane's bounded visible content in a placement preview.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PanePlacementPaneSnapshot {
    /// The pane this content belongs to.
    pub pane_id: PaneId,
    /// The visible terminal cells, without scrollback metadata.
    pub terminal_window: Option<FrameWindow>,
    /// Native-image placements whose records may follow in bounded events.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub image_placement_snapshots: Vec<FrameImagePlacement>,
}

/// The requesting client's retained view inputs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PanePlacementClientSnapshot {
    /// The requesting client.
    pub client_id: ClientId,
    /// The client's outer terminal size.
    pub viewport_size: Size,
    /// The pane area reported by the client, if any.
    #[serde(default)]
    pub pane_area: Option<PaneArea>,
    /// The tab the client viewed when it requested the preview.
    pub active_tab_id: TabId,
    /// The pane focused in the active tab, if any.
    #[serde(default)]
    pub focused_pane_id: Option<PaneId>,
}

/// The shared layout sizing inputs used for a placement preview.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PanePlacementSizing {
    /// The minimum pane size used by the solver.
    pub minimum_size: Size,
    /// The blank-cell gap between adjacent split panes.
    pub gap_cell_count: u16,
}

/// Why a decoded placement preview is not safe to retain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanePlacementSnapshotValidationError {
    /// The source or destination tab id disagrees with its record.
    TabIdentityMismatch,
    /// The requested source pane is not in the source tab.
    SourcePaneMissing,
    /// The preview has too many panes.
    PaneCountExceeded,
    /// The preview has too many cells.
    CellCountExceeded,
    /// The preview has too many image placements.
    ImageCountExceeded,
    /// The preview references too many image bytes.
    ImageByteCountExceeded,
    /// The same pane id appears more than once in the preview.
    DuplicatePaneId,
    /// The layout tree is not a bounded, canonical tree for the pane slots.
    LayoutMismatch,
    /// The preview does not carry one pane snapshot for each pane slot.
    PaneSnapshotMismatch,
    /// A pane slot has inconsistent geometry or visibility flags.
    PaneSlotMismatch,
    /// The terminal window has invalid bounded grid data.
    TerminalWindowMismatch,
    /// An image placement is malformed or outside its terminal window.
    ImagePlacementMismatch,
    /// One image content id carries different pixel dimensions.
    ImageIdentityMismatch,
    /// A pane repeats one terminal-local image placement identity.
    DuplicateImagePlacementId,
}

fn validate_tab_snapshot(
    tab_snapshot: &PanePlacementTabSnapshot,
    placement_validation: &mut PanePlacementValidationState,
    retained_source_pane_id: Option<PaneId>,
) -> Result<(), PanePlacementSnapshotValidationError> {
    if tab_snapshot.pane_slots.len() > MAX_PLACEMENT_SNAPSHOT_PANE_COUNT
        || tab_snapshot.pane_snapshots.len() > MAX_PLACEMENT_SNAPSHOT_PANE_COUNT
        || placement_validation
            .pane_count
            .checked_add(tab_snapshot.pane_slots.len())
            .is_none_or(|count| count > MAX_PLACEMENT_SNAPSHOT_PANE_COUNT)
    {
        return Err(PanePlacementSnapshotValidationError::PaneCountExceeded);
    }
    if tab_snapshot.layout_mode != LayoutMode::Tiled
        || tab_snapshot.gap_cell_count != placement_validation.expected_gap_cell_count
    {
        return Err(PanePlacementSnapshotValidationError::LayoutMismatch);
    }
    if tab_snapshot.pane_slots.len() != tab_snapshot.pane_snapshots.len() {
        return Err(PanePlacementSnapshotValidationError::PaneSnapshotMismatch);
    }

    let layout_pane_ids = collect_layout_pane_ids(&tab_snapshot.layout_tree)?;
    let slot_pane_ids: Vec<PaneId> = tab_snapshot
        .pane_slots
        .iter()
        .map(|pane_slot| pane_slot.pane_id)
        .collect();
    for pane_id in &slot_pane_ids {
        if !placement_validation.pane_ids.insert(*pane_id) {
            return Err(PanePlacementSnapshotValidationError::DuplicatePaneId);
        }
    }
    if layout_pane_ids != slot_pane_ids {
        return Err(PanePlacementSnapshotValidationError::LayoutMismatch);
    }

    let mut pane_snapshot_ids = HashSet::new();
    for pane_snapshot in &tab_snapshot.pane_snapshots {
        if !pane_snapshot_ids.insert(pane_snapshot.pane_id) {
            return Err(PanePlacementSnapshotValidationError::DuplicatePaneId);
        }
    }
    let snapshot_pane_ids: Vec<PaneId> = tab_snapshot
        .pane_snapshots
        .iter()
        .map(|pane_snapshot| pane_snapshot.pane_id)
        .collect();
    if snapshot_pane_ids != slot_pane_ids {
        return Err(PanePlacementSnapshotValidationError::PaneSnapshotMismatch);
    }

    for pane_slot in &tab_snapshot.pane_slots {
        if !is_rect_within_size(pane_slot.outer_rect, tab_snapshot.effective_cell_size)
            || pane_slot.is_visible != pane_slot.content_rect.is_some()
            || pane_slot.content_rect.is_some_and(|content_rect| {
                content_rect.is_empty()
                    || !is_rect_within_rect(content_rect, pane_slot.outer_rect)
                    || !is_rect_within_size(content_rect, tab_snapshot.effective_cell_size)
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
    validate_stack_headers(tab_snapshot)?;

    placement_validation.pane_count = placement_validation
        .pane_count
        .checked_add(tab_snapshot.pane_slots.len())
        .ok_or(PanePlacementSnapshotValidationError::PaneCountExceeded)?;
    for (pane_slot, pane_snapshot) in tab_snapshot
        .pane_slots
        .iter()
        .zip(&tab_snapshot.pane_snapshots)
    {
        validate_pane_snapshot(
            pane_slot,
            pane_snapshot,
            placement_validation,
            retained_source_pane_id == Some(pane_slot.pane_id),
        )?;
    }
    Ok(())
}

fn collect_layout_pane_ids(
    layout_tree: &LayoutNode,
) -> Result<Vec<PaneId>, PanePlacementSnapshotValidationError> {
    let mut layout_nodes = vec![layout_tree];
    let mut layout_pane_ids = Vec::new();
    let mut layout_node_count = 0usize;
    while let Some(layout_node) = layout_nodes.pop() {
        layout_node_count = layout_node_count
            .checked_add(1)
            .ok_or(PanePlacementSnapshotValidationError::LayoutMismatch)?;
        if layout_node_count > MAX_PLACEMENT_SNAPSHOT_LAYOUT_NODE_COUNT {
            return Err(PanePlacementSnapshotValidationError::PaneCountExceeded);
        }
        match layout_node {
            LayoutNode::Pane(pane_id) => {
                layout_pane_ids.push(*pane_id);
                if layout_pane_ids.len() > MAX_PLACEMENT_SNAPSHOT_PANE_COUNT {
                    return Err(PanePlacementSnapshotValidationError::PaneCountExceeded);
                }
            }
            LayoutNode::Split(split_node) => {
                if split_node.children.len() < 2
                    || split_node.children.len() > MAX_PLACEMENT_SNAPSHOT_PANE_COUNT
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
                if layout_node_count
                    .checked_add(layout_nodes.len())
                    .is_none_or(|count| count > MAX_PLACEMENT_SNAPSHOT_LAYOUT_NODE_COUNT)
                {
                    return Err(PanePlacementSnapshotValidationError::PaneCountExceeded);
                }
            }
        }
    }
    Ok(layout_pane_ids)
}

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

fn validate_stack_headers(
    tab_snapshot: &PanePlacementTabSnapshot,
) -> Result<(), PanePlacementSnapshotValidationError> {
    if tab_snapshot.stack_headers.len() > tab_snapshot.pane_slots.len() {
        return Err(PanePlacementSnapshotValidationError::LayoutMismatch);
    }
    let pane_ids: HashSet<PaneId> = tab_snapshot
        .pane_slots
        .iter()
        .map(|pane_slot| pane_slot.pane_id)
        .collect();
    let mut header_pane_ids = HashSet::new();
    for stack_header in &tab_snapshot.stack_headers {
        if !pane_ids.contains(&stack_header.pane_id)
            || !header_pane_ids.insert(stack_header.pane_id)
            || stack_header.member_count < 2
            || stack_header.member_index >= stack_header.member_count
            || !is_rect_within_size(stack_header.header_rect, tab_snapshot.effective_cell_size)
        {
            return Err(PanePlacementSnapshotValidationError::LayoutMismatch);
        }
    }
    Ok(())
}

fn validate_pane_snapshot(
    pane_slot: &FrameSlot,
    pane_snapshot: &PanePlacementPaneSnapshot,
    placement_validation: &mut PanePlacementValidationState,
    is_retained_source_pane: bool,
) -> Result<(), PanePlacementSnapshotValidationError> {
    if matches!(pane_slot.pane_kind, PaneKind::Plugin { .. })
        && pane_snapshot.terminal_window.is_some()
    {
        return Err(PanePlacementSnapshotValidationError::TerminalWindowMismatch);
    }
    if pane_slot.content_rect.is_none()
        && !is_retained_source_pane
        && (pane_snapshot.terminal_window.is_some()
            || !pane_snapshot.image_placement_snapshots.is_empty())
    {
        return Err(PanePlacementSnapshotValidationError::PaneSlotMismatch);
    }
    if !pane_snapshot.image_placement_snapshots.is_empty()
        && pane_snapshot.terminal_window.is_none()
    {
        return Err(PanePlacementSnapshotValidationError::ImagePlacementMismatch);
    }
    if let Some(terminal_window) = &pane_snapshot.terminal_window {
        validate_terminal_window(terminal_window, &mut placement_validation.cell_count)?;
    }
    placement_validation.image_count = placement_validation
        .image_count
        .checked_add(pane_snapshot.image_placement_snapshots.len())
        .ok_or(PanePlacementSnapshotValidationError::ImageCountExceeded)?;
    if placement_validation.image_count > MAX_PLACEMENT_SNAPSHOT_IMAGE_COUNT {
        return Err(PanePlacementSnapshotValidationError::ImageCountExceeded);
    }
    let mut image_placement_ids = HashSet::new();
    for image_placement in &pane_snapshot.image_placement_snapshots {
        if !image_placement_ids.insert(image_placement.placement_id) {
            return Err(PanePlacementSnapshotValidationError::DuplicateImagePlacementId);
        }
        validate_image_placement(
            image_placement,
            pane_snapshot
                .terminal_window
                .as_ref()
                .ok_or(PanePlacementSnapshotValidationError::ImagePlacementMismatch)?,
            placement_validation,
        )?;
    }
    Ok(())
}

fn validate_terminal_window(
    terminal_window: &FrameWindow,
    cell_count: &mut u64,
) -> Result<(), PanePlacementSnapshotValidationError> {
    if terminal_window.column_count == 0
        || terminal_window.row_snapshots.is_empty()
        || terminal_window.row_snapshots.len() > usize::from(u16::MAX)
        || terminal_window.view_row_offset != 0
    {
        return Err(PanePlacementSnapshotValidationError::TerminalWindowMismatch);
    }
    for frame_row in &terminal_window.row_snapshots {
        let mut row_cell_count = 0u64;
        for frame_run in &frame_row.cell_runs {
            if frame_run.repeat_count == 0
                || frame_run.cell.cell_width > 2
                || frame_run.cell.combining_characters.len()
                    > MAX_PLACEMENT_SNAPSHOT_COMBINING_CHARACTER_COUNT
            {
                return Err(PanePlacementSnapshotValidationError::TerminalWindowMismatch);
            }
            row_cell_count = row_cell_count
                .checked_add(u64::from(frame_run.repeat_count))
                .ok_or(PanePlacementSnapshotValidationError::CellCountExceeded)?;
        }
        if row_cell_count != u64::from(terminal_window.column_count) {
            return Err(PanePlacementSnapshotValidationError::TerminalWindowMismatch);
        }
        *cell_count = cell_count
            .checked_add(row_cell_count)
            .ok_or(PanePlacementSnapshotValidationError::CellCountExceeded)?;
        if *cell_count > MAX_PLACEMENT_SNAPSHOT_CELL_COUNT {
            return Err(PanePlacementSnapshotValidationError::CellCountExceeded);
        }
    }
    Ok(())
}

fn validate_image_placement(
    image_placement: &FrameImagePlacement,
    terminal_window: &FrameWindow,
    placement_validation: &mut PanePlacementValidationState,
) -> Result<(), PanePlacementSnapshotValidationError> {
    let terminal_window_size = Size {
        column_count: terminal_window.column_count,
        row_count: u16::try_from(terminal_window.row_snapshots.len())
            .map_err(|_| PanePlacementSnapshotValidationError::TerminalWindowMismatch)?,
    };
    let image_cell_size = Size {
        column_count: image_placement.column_count,
        row_count: image_placement.row_count,
    };
    if image_placement.placement_id == 0
        || image_placement.image_content_id == 0
        || image_placement.column_count == 0
        || image_placement.row_count == 0
        || !is_cell_rect_within(
            image_placement.anchor_cell,
            image_cell_size,
            terminal_window_size,
        )
        || image_placement
            .cell_geometry
            .is_some_and(|cell_geometry| !cell_geometry.is_visible_size_contained(image_cell_size))
        || image_placement
            .image_record
            .as_ref()
            .is_some_and(|image_record| image_record.image_action == FrameImageAction::Transmit)
    {
        return Err(PanePlacementSnapshotValidationError::ImagePlacementMismatch);
    }
    if let Some(image_record) = &image_placement.image_record {
        let image_dimensions = (image_record.pixel_width, image_record.pixel_height);
        match placement_validation
            .image_dimensions_by_content_id
            .entry(image_placement.image_content_id)
        {
            Entry::Occupied(existing_image_dimensions_entry) => {
                if *existing_image_dimensions_entry.get() != image_dimensions {
                    return Err(PanePlacementSnapshotValidationError::ImageIdentityMismatch);
                }
            }
            Entry::Vacant(vacant_image_dimensions_entry) => {
                vacant_image_dimensions_entry.insert(image_dimensions);
                let image_record_byte_count = compute_frame_image_byte_count(
                    image_record.pixel_width,
                    image_record.pixel_height,
                )
                .map_err(|_| PanePlacementSnapshotValidationError::ImagePlacementMismatch)?;
                placement_validation.image_byte_count = placement_validation
                    .image_byte_count
                    .checked_add(image_record_byte_count)
                    .ok_or(PanePlacementSnapshotValidationError::ImageByteCountExceeded)?;
                if placement_validation.image_byte_count > MAX_PLACEMENT_SNAPSHOT_IMAGE_BYTE_COUNT {
                    return Err(PanePlacementSnapshotValidationError::ImageByteCountExceeded);
                }
            }
        }
    }
    Ok(())
}

fn is_rect_within_size(rect: Rect, size: Size) -> bool {
    u32::from(rect.origin.column) + u32::from(rect.cell_size.column_count)
        <= u32::from(size.column_count)
        && u32::from(rect.origin.row) + u32::from(rect.cell_size.row_count)
            <= u32::from(size.row_count)
}

fn is_rect_within_rect(inner_rect: Rect, outer_rect: Rect) -> bool {
    inner_rect.origin.column >= outer_rect.origin.column
        && inner_rect.origin.row >= outer_rect.origin.row
        && u32::from(inner_rect.origin.column) + u32::from(inner_rect.cell_size.column_count)
            <= u32::from(outer_rect.origin.column) + u32::from(outer_rect.cell_size.column_count)
        && u32::from(inner_rect.origin.row) + u32::from(inner_rect.cell_size.row_count)
            <= u32::from(outer_rect.origin.row) + u32::from(outer_rect.cell_size.row_count)
}

fn is_cell_rect_within(anchor_cell: (u16, u16), cell_size: Size, content_size: Size) -> bool {
    u32::from(anchor_cell.0) + u32::from(cell_size.row_count) <= u32::from(content_size.row_count)
        && u32::from(anchor_cell.1) + u32::from(cell_size.column_count)
            <= u32::from(content_size.column_count)
}

#[cfg(test)]
mod tests;
