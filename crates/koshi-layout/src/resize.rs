//! The resize transaction: move one pane border by an exact cell count.
//!
//! A resize permanently shifts cells between two siblings by updating their
//! weights' `resize_delta`, then lets the solver re-derive geometry.
//!
//! The signed cell delta names the border by direction: `resize_layout(pane,
//! Right, 5)` moves the pane's right border outward (the pane grows,
//! the right neighbor donates), and `resize_layout(pane, Right, -5)` moves the
//! same border inward (the pane donates, the right neighbor gains).
//!
//! Panes inside a stack resize as a unit: the border that moves is the
//! stack's outer one, never a border between two stack members.

use koshi_core::error::{DomainCategory, DomainError, Severity};
use koshi_core::geometry::{Direction, Rect, SplitDirection};
use koshi_core::ids::PaneId;
use thiserror::Error;

use crate::size::SizeWeight;
use crate::solver::{
    compute_directional_child_rects, compute_slot_floor, compute_stacked_child_rects, PaneSizing,
};
use crate::tree::{compute_split_direction, LayoutNode};

/// A rejected resize. The caller's tree is unchanged in every case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ResizeError {
    /// The pane to resize is not in this layout.
    #[error("pane {pane_id} is not in this layout")]
    PaneNotFound { pane_id: PaneId },
    /// No border exists on that side: the pane touches the tab edge there
    /// at every level of the tree.
    #[error("pane {pane_id} has no {direction:?} border to adjust")]
    NoAdjacentBorder {
        pane_id: PaneId,
        direction: Direction,
    },
    /// The pane giving up the cells — the neighbor on a grow, the pane
    /// itself on a shrink — cannot give that many without going below its
    /// minimum size.
    #[error(
        "resize of {requested_cell_count} cells exceeds the donating pane's {spare_cell_count} spare cells"
    )]
    MinimumSizeExceeded {
        requested_cell_count: u16,
        spare_cell_count: u16,
    },
}

impl DomainError for ResizeError {
    fn category(&self) -> DomainCategory {
        DomainCategory::Layout
    }

    fn get_severity(&self) -> Severity {
        Severity::Recoverable
    }
}

/// Moves `pane_id`'s border on the `direction` side by `cell_delta`: positive
/// moves it outward (the pane grows and the adjacent sibling on that side
/// donates the cells), negative moves it inward (the pane donates and that
/// sibling gains them). A `cell_delta` of `0` runs the same lookups and checks
/// and moves no cells.
///
/// The border that moves belongs to the deepest ancestor split that runs on
/// the matching axis (horizontal for left/right, vertical for up/down) and
/// has a sibling on the `direction` side. A pane on its inner split's edge
/// moves the enclosing split's border. Splits below a collapsed stack
/// member are skipped. `tab_rect` is the rect the tree solves into; the
/// donor's solved size above its floor bounds the move.
///
/// # Errors
///
/// - [`ResizeError::PaneNotFound`] when `pane_id` is not in the tree.
/// - [`ResizeError::NoAdjacentBorder`] when no ancestor has a neighbor on
///   that side.
/// - [`ResizeError::MinimumSizeExceeded`] when the donating side would drop below its
///   floor.
pub fn resize_layout(
    layout_tree: &LayoutNode,
    tab_rect: Rect,
    pane_id: PaneId,
    direction: Direction,
    cell_delta: i16,
) -> Result<LayoutNode, ResizeError> {
    resize_layout_with_sizing(
        layout_tree,
        tab_rect,
        pane_id,
        direction,
        cell_delta,
        PaneSizing::default(),
    )
}

/// Like [`resize_layout`] with an explicit [`PaneSizing`]: `pane_sizing.minimum_size` is the
/// per-pane content floor the donor's spare is measured against, and the
/// donor's solved size excludes the [`PaneSizing::gap_cell_count`] beside it.
/// [`resize_layout`] passes [`PaneSizing::default`].
pub fn resize_layout_with_sizing(
    layout_tree: &LayoutNode,
    tab_rect: Rect,
    pane_id: PaneId,
    direction: Direction,
    cell_delta: i16,
    pane_sizing: PaneSizing,
) -> Result<LayoutNode, ResizeError> {
    let pane_path = layout_tree
        .find_pane_path(pane_id)
        .ok_or(ResizeError::PaneNotFound { pane_id })?;

    let split_direction = compute_split_direction(direction);
    let is_horizontal_split = split_direction == SplitDirection::Horizontal;

    // The deepest ancestor split on the wanted axis with a neighbor on the
    // resize side owns the border being moved.
    let (ancestor_depth, target_child_index, neighbor_child_index) =
        find_resize_border(layout_tree, &pane_path, split_direction, direction)
            .ok_or(ResizeError::NoAdjacentBorder { pane_id, direction })?;

    // The sign picks who donates the cells across the border: on a grow the
    // neighbor gives them to the pane, on a shrink the pane gives them to
    // the neighbor.
    let requested_cell_count = cell_delta.unsigned_abs();
    let (receiving_child_index, donating_child_index) = if cell_delta < 0 {
        (neighbor_child_index, target_child_index)
    } else {
        (target_child_index, neighbor_child_index)
    };

    // The donor can give only what its solved size holds above its floor.
    let split_node = layout_tree.get_split_at_path(&pane_path[..ancestor_depth]);
    let split_rect = compute_rect_at_path(
        layout_tree,
        tab_rect,
        &pane_path[..ancestor_depth],
        pane_sizing,
    );
    let donating_rect =
        compute_directional_child_rects(split_node, split_rect, pane_sizing)[donating_child_index];
    let donating_cell_count = if is_horizontal_split {
        donating_rect.cell_size.column_count
    } else {
        donating_rect.cell_size.row_count
    };
    let spare_cell_count = donating_cell_count.saturating_sub(compute_slot_floor(
        split_node,
        donating_child_index,
        is_horizontal_split,
        pane_sizing,
    ));
    if requested_cell_count > spare_cell_count {
        return Err(ResizeError::MinimumSizeExceeded {
            requested_cell_count,
            spare_cell_count,
        });
    }

    let mut updated_layout_tree = layout_tree.clone();
    let split_node = updated_layout_tree.get_split_at_path_mut(&pane_path[..ancestor_depth]);
    // Missing weights are padded with the default share up to the child
    // count.
    if split_node.weights.len() < split_node.children.len() {
        split_node
            .weights
            .resize(split_node.children.len(), SizeWeight::default());
    }
    split_node.weights[receiving_child_index].resize_delta = split_node.weights
        [receiving_child_index]
        .resize_delta
        .saturating_add(i32::from(requested_cell_count));
    split_node.weights[donating_child_index].resize_delta = split_node.weights
        [donating_child_index]
        .resize_delta
        .saturating_sub(i32::from(requested_cell_count));
    Ok(updated_layout_tree)
}

/// The deepest ancestor split of direction `split_direction`, above any collapsed
/// stack member on `pane_path`, whose path child has a sibling on the `direction`
/// side: its depth in `pane_path`, the path child's index, and the sibling's
/// index. `None` when no such split exists.
fn find_resize_border(
    layout_tree: &LayoutNode,
    pane_path: &[usize],
    split_direction: SplitDirection,
    direction: Direction,
) -> Option<(usize, usize, usize)> {
    // Only splits above the first stacked split whose path child is
    // collapsed are candidates.
    let mut visible_path_length = pane_path.len();
    let mut layout_node = layout_tree;
    for (path_depth, &child_index) in pane_path.iter().enumerate() {
        let LayoutNode::Split(split) = layout_node else {
            break;
        };
        if split.direction == SplitDirection::Stacked
            && child_index != split.get_active_child_index()
        {
            visible_path_length = path_depth;
            break;
        }
        layout_node = &split.children[child_index];
    }

    for ancestor_depth in (0..visible_path_length).rev() {
        let split_node = layout_tree.get_split_at_path(&pane_path[..ancestor_depth]);
        if split_node.direction != split_direction {
            continue;
        }
        let target_child_index = pane_path[ancestor_depth];
        let neighbor_child_index = match direction {
            Direction::Left | Direction::Up => target_child_index.checked_sub(1),
            Direction::Right | Direction::Down => (target_child_index + 1
                < split_node.children.len())
            .then_some(target_child_index + 1),
        };
        if let Some(neighbor_child_index) = neighbor_child_index {
            return Some((ancestor_depth, target_child_index, neighbor_child_index));
        }
    }
    None
}

/// The rect the node at `pane_path` solves into, starting from `tab_rect`.
///
/// A directional level takes the child rect [`directional_child_rects`]
/// derives; a stacked level the child rect [`stacked_child_rects`] derives.
fn compute_rect_at_path(
    layout_tree: &LayoutNode,
    tab_rect: Rect,
    pane_path: &[usize],
    pane_sizing: PaneSizing,
) -> Rect {
    let mut layout_node = layout_tree;
    let mut current_rect = tab_rect;
    for &child_index in pane_path {
        let LayoutNode::Split(split) = layout_node else {
            unreachable!("pane path was built over this tree");
        };
        current_rect = match split.direction {
            SplitDirection::Horizontal | SplitDirection::Vertical => {
                compute_directional_child_rects(split, current_rect, pane_sizing)[child_index]
            }
            SplitDirection::Stacked => {
                compute_stacked_child_rects(split, current_rect, pane_sizing)[child_index]
            }
        };
        layout_node = &split.children[child_index];
    }
    current_rect
}

#[cfg(test)]
mod tests;
