//! The resize transaction: move one pane border by up to a signed cell count.
//!
//! A resize permanently shifts cells between two siblings by updating their
//! weights' `resize_delta`, then lets the solver re-derive geometry.
//!
//! The signed cell delta names the border by direction: a resize of `pane`
//! toward `Right` by `5` moves the pane's right border outward (the pane
//! grows, the right neighbor donates), and by `-5` moves the same border
//! inward (the pane donates, the right neighbor gains). The cell count is
//! the distance the border moves on screen; the border stops early when the
//! donor reaches its minimum size or the layout holds it in place.
//!
//! Panes inside a stack resize as a unit: the border that moves is the
//! stack's outer one, never a border between two stack members.

use koshi_core::geometry::{Direction, Rect, SplitDirection};
use koshi_core::ids::PaneId;
use thiserror::Error;

use crate::size::SizeWeight;
use crate::solver::{
    compute_directional_child_rects, compute_slot_floor, compute_stacked_child_rects, PaneSizing,
};
use crate::tree::{compute_split_direction, LayoutNode, SplitNode};

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
    /// The side giving up the cells — the neighbor on a grow, the pane
    /// itself on a shrink — holds no cell above its minimum size: it is at
    /// that size, or suppressed for lack of room. The `direction` border of
    /// `pane_id` cannot move one cell. `is_pane_growing` is `true` for a grow
    /// and `false` for a shrink.
    #[error("the {direction:?} border of pane {pane_id} cannot move: the donating side has no room to give")]
    DonorAtMinimumSize {
        pane_id: PaneId,
        direction: Direction,
        is_pane_growing: bool,
    },
    /// The donating side is above its minimum size, but no added resize
    /// offset moves the `direction` border of `pane_id` one cell on screen.
    /// `is_pane_growing` is `true` for a grow and `false` for a shrink.
    ///
    /// Three layouts hold a border:
    /// - a child of the split keeps its `preferred` size: a pane at
    ///   `preferred 20` beside a wider flexible pane keeps its 20 columns,
    ///   whichever side grows;
    /// - the receiving side's share plus its offset is below its floor, and
    ///   a third child gives the cells that lift it to that floor: each
    ///   offset cell goes from the donor to that third child until the donor
    ///   is at its floor;
    /// - the receiving side is suppressed for lack of room.
    #[error(
        "the {direction:?} border of pane {pane_id} cannot move: the layout holds it in place"
    )]
    BorderHeld {
        pane_id: PaneId,
        direction: Direction,
        is_pane_growing: bool,
    },
}

/// Moves `pane_id`'s border on the `direction` side by up to `resize_cell_delta`
/// cells on screen: positive moves it outward (the pane grows and the
/// adjacent sibling on that side donates the cells), negative moves it inward
/// (the pane donates and that sibling gains them).
///
/// The border that moves belongs to the deepest ancestor split that runs on
/// the matching axis (horizontal for left/right, vertical for up/down) and
/// has a sibling on the `direction` side. A pane on its inner split's edge
/// moves the enclosing split's border. Splits below a collapsed stack
/// member are skipped. `tab_rect` is the rect the tree solves into.
///
/// The move adds one cell of resize offset at a time to the receiving
/// sibling and takes one from the donating sibling, then solves the split
/// again and measures how far the border moved from where it started. Only
/// the added counts at which the receiver's or the donor's offset lies
/// between minus and plus the split's axis length are solved, at most four
/// times that length; the solve at any other count equals the solve at the
/// count before it. The offsets stop when the border moved the whole
/// magnitude of `resize_cell_delta`, when the donor's solved size holds no
/// cell above its floor, or after the last of those counts. The resized tree
/// keeps only the offset that the last on-screen cell of the move needed. One
/// offset cell can move the border by no cell when a floor holds the
/// receiver up; the next offset cell then moves it.
///
/// `pane_sizing.minimum_size` is the per-pane content floor the donor's spare
/// is measured against, and the donor's solved size excludes the
/// [`PaneSizing::gap_cell_count`] beside it.
///
/// Returns the resized tree and the cell count the border moved on screen,
/// at most the magnitude of `resize_cell_delta`. A donor `10` columns wide
/// with a floor of `4`, asked for `100`, moves `6`. A `resize_cell_delta` of
/// `0` runs the same lookups, moves no cells, and returns `0`.
///
/// # Errors
///
/// - [`ResizeError::PaneNotFound`] when `pane_id` is not in the tree.
/// - [`ResizeError::NoAdjacentBorder`] when no ancestor has a neighbor on
///   that side.
/// - [`ResizeError::DonorAtMinimumSize`] when `resize_cell_delta` is not `0`
///   and the donor holds no cell above its floor.
/// - [`ResizeError::BorderHeld`] when `resize_cell_delta` is not `0`, the
///   donor holds a cell above its floor, and no offset moves the border one
///   cell.
pub fn resize_layout_with_sizing(
    layout_tree: &LayoutNode,
    tab_rect: Rect,
    pane_id: PaneId,
    direction: Direction,
    resize_cell_delta: i16,
    pane_sizing: PaneSizing,
) -> Result<(LayoutNode, u16), ResizeError> {
    let pane_path = layout_tree
        .find_pane_path(pane_id)
        .ok_or(ResizeError::PaneNotFound { pane_id })?;

    let split_direction = compute_split_direction(direction);
    let is_horizontal_split = split_direction == SplitDirection::Horizontal;

    // The deepest ancestor split on the wanted axis with a neighbor on the
    // resize side owns the border being moved.
    let (resize_split_depth, target_pane_child_index, adjacent_child_index) =
        find_resize_border(layout_tree, &pane_path, split_direction, direction)
            .ok_or(ResizeError::NoAdjacentBorder { pane_id, direction })?;

    // The sign picks who donates the cells across the border: on a grow the
    // neighbor gives them to the pane, on a shrink the pane gives them to
    // the neighbor.
    let requested_cell_count = resize_cell_delta.unsigned_abs();
    let is_pane_growing = resize_cell_delta > 0;
    let (receiving_child_index, donating_child_index) = if is_pane_growing {
        (target_pane_child_index, adjacent_child_index)
    } else {
        (adjacent_child_index, target_pane_child_index)
    };

    let split_path = &pane_path[..resize_split_depth];
    let split_rect = compute_rect_at_path(layout_tree, tab_rect, split_path, pane_sizing);
    let mut updated_layout_tree = layout_tree.clone();
    let split_node = updated_layout_tree.get_split_at_path_mut(split_path);
    // Missing weights are padded with the default share up to the child
    // count.
    if split_node.weights.len() < split_node.children.len() {
        split_node
            .weights
            .resize(split_node.children.len(), SizeWeight::default());
    }
    let donating_slot_floor = compute_slot_floor(
        split_node,
        donating_child_index,
        is_horizontal_split,
        pane_sizing,
    );
    // The border is the leading edge of whichever sibling comes second in
    // layout order.
    let border_child_index = receiving_child_index.max(donating_child_index);
    let is_border_moving_forward = donating_child_index > receiving_child_index;
    // The border's cell on the split axis, and the cells the donor's solved
    // size holds above its floor. A floor above `u16::MAX` leaves no spare
    // cell.
    let measure_border = |split_node: &SplitNode| -> (i32, u16) {
        let child_rects = compute_directional_child_rects(split_node, split_rect, pane_sizing);
        let border_rect = child_rects[border_child_index];
        let donating_pane_rect = child_rects[donating_child_index];
        let (border_cell, donating_cell_count) = if is_horizontal_split {
            (
                border_rect.origin.column,
                donating_pane_rect.size.column_count,
            )
        } else {
            (border_rect.origin.row, donating_pane_rect.size.row_count)
        };
        let spare_cell_count = match u16::try_from(donating_slot_floor) {
            Ok(donating_slot_floor) => donating_cell_count.saturating_sub(donating_slot_floor),
            Err(_) => 0,
        };
        (i32::from(border_cell), spare_cell_count)
    };

    let starting_receiving_resize_delta =
        i64::from(split_node.weights[receiving_child_index].resize_delta);
    let starting_donating_resize_delta =
        i64::from(split_node.weights[donating_child_index].resize_delta);
    // The receiver gains `added_cell_count` cells of offset and the donor
    // loses as many. An offset outside the `i32` range is stored at its end.
    let set_added_cell_count = |split_node: &mut SplitNode, added_cell_count: i64| {
        split_node.weights[receiving_child_index].resize_delta =
            (starting_receiving_resize_delta + added_cell_count)
                .clamp(i32::MIN.into(), i32::MAX.into()) as i32;
        split_node.weights[donating_child_index].resize_delta =
            (starting_donating_resize_delta - added_cell_count)
                .clamp(i32::MIN.into(), i32::MAX.into()) as i32;
    };
    // The solver clamps each share plus its offset to the cells the children
    // share, at most `axis_cell_count`, and every share lies in that range:
    // an offset changes the solve only inside
    // `-axis_cell_count..=axis_cell_count`. The receiver's offset crosses
    // that range at the added counts in `receiving_offset_window`, the
    // donor's at those in `donating_offset_window`. Every other added count
    // solves like the count before it and is skipped. The two windows are
    // walked in ascending order, each count once.
    let axis_cell_count = i64::from(if is_horizontal_split {
        split_rect.size.column_count
    } else {
        split_rect.size.row_count
    });
    let receiving_offset_window = (1 - axis_cell_count - starting_receiving_resize_delta).max(1)
        ..=(axis_cell_count - starting_receiving_resize_delta);
    let donating_offset_window = (1 - axis_cell_count + starting_donating_resize_delta).max(1)
        ..=(axis_cell_count + starting_donating_resize_delta);
    let (first_offset_window, second_offset_window) =
        if receiving_offset_window.start() <= donating_offset_window.start() {
            (receiving_offset_window, donating_offset_window)
        } else {
            (donating_offset_window, receiving_offset_window)
        };
    let added_cell_counts = first_offset_window.clone().chain(
        second_offset_window
            .filter(|added_cell_count| !first_offset_window.contains(added_cell_count)),
    );
    let (starting_border_cell, starting_spare_cell_count) = measure_border(split_node);

    let mut best_moved_cell_count: u16 = 0;
    let mut best_added_cell_count: i64 = 0;
    if requested_cell_count > 0 && starting_spare_cell_count > 0 {
        for added_cell_count in added_cell_counts {
            set_added_cell_count(split_node, added_cell_count);
            let (border_cell, spare_cell_count) = measure_border(split_node);
            let moved_cell_count = if is_border_moving_forward {
                border_cell - starting_border_cell
            } else {
                starting_border_cell - border_cell
            };
            if moved_cell_count > i32::from(best_moved_cell_count)
                && moved_cell_count <= i32::from(requested_cell_count)
            {
                best_moved_cell_count = moved_cell_count as u16;
                best_added_cell_count = added_cell_count;
            }
            if moved_cell_count >= i32::from(requested_cell_count) || spare_cell_count == 0 {
                break;
            }
        }
    }

    if requested_cell_count > 0 && best_moved_cell_count == 0 {
        return Err(if starting_spare_cell_count == 0 {
            ResizeError::DonorAtMinimumSize {
                pane_id,
                direction,
                is_pane_growing,
            }
        } else {
            ResizeError::BorderHeld {
                pane_id,
                direction,
                is_pane_growing,
            }
        });
    }
    set_added_cell_count(split_node, best_added_cell_count);
    Ok((updated_layout_tree, best_moved_cell_count))
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
        let pane_path_child_index = pane_path[ancestor_depth];
        let adjacent_child_index = match direction {
            Direction::Left | Direction::Up => pane_path_child_index.checked_sub(1),
            Direction::Right | Direction::Down => (pane_path_child_index + 1
                < split_node.children.len())
            .then_some(pane_path_child_index + 1),
        };
        if let Some(adjacent_child_index) = adjacent_child_index {
            return Some((ancestor_depth, pane_path_child_index, adjacent_child_index));
        }
    }
    None
}

/// The rect the node at `pane_path` solves into, starting from `tab_rect`.
///
/// A directional level takes the child rect [`compute_directional_child_rects`]
/// derives; a stacked level the child rect [`compute_stacked_child_rects`] derives.
fn compute_rect_at_path(
    layout_tree: &LayoutNode,
    tab_rect: Rect,
    pane_path: &[usize],
    pane_sizing: PaneSizing,
) -> Rect {
    let mut layout_node = layout_tree;
    let mut current_layout_node_rect = tab_rect;
    for &child_index in pane_path {
        let LayoutNode::Split(split) = layout_node else {
            unreachable!("pane path was built over this tree");
        };
        current_layout_node_rect = match split.direction {
            SplitDirection::Horizontal | SplitDirection::Vertical => {
                compute_directional_child_rects(split, current_layout_node_rect, pane_sizing)
                    [child_index]
            }
            SplitDirection::Stacked => {
                compute_stacked_child_rects(split, current_layout_node_rect, pane_sizing)
                    [child_index]
            }
        };
        layout_node = &split.children[child_index];
    }
    current_layout_node_rect
}

#[cfg(test)]
mod tests;
