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
    /// Two layouts hold a border:
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
/// The move adds a count of resize offset cells to the receiving sibling and
/// takes as many from the donating sibling, then solves the split again and
/// measures how far the border moved from where it started. The candidate
/// counts are those at which the receiver's or the donor's offset lies
/// between minus and plus the split's axis length, at most four times that
/// length; the solve at any other count equals the solve at the count before
/// it. While the donor holds a cell above its floor, one more added cell
/// moves the border by no cell or by one cell in the asked direction. Once
/// the donor holds no cell above its floor, it holds none at any larger
/// count. One offset cell moves the border by no cell when a floor holds the
/// receiver up; the next offset cell then moves it.
///
/// The move stops at the first candidate at which the border moved the whole
/// magnitude of `resize_cell_delta` or the donor's solved size holds no cell
/// above its floor, else at the last candidate. The resized tree keeps the
/// smallest added count that moves the border as far as that stop. The split
/// is solved at the candidate the magnitude names, then at the last
/// candidate when the border has not stopped, then at the middle of the
/// range left until one candidate remains: a one-cell move solves the split
/// twice, a border no candidate moves three times, and `100` asked of a
/// 200-column split of two equal panes, whose donor holds `96` spare cells,
/// nine times.
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
    // solves like the count before it and is no candidate.
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
    // Candidate number `n`, counted from 1, is the `n`th count of the first
    // window, then of the second window's counts past the first window's end.
    let first_window_length = (first_offset_window.end() - first_offset_window.start() + 1).max(0);
    let second_window_start = (*second_offset_window.start()).max(first_offset_window.end() + 1);
    let second_window_length = (second_offset_window.end() - second_window_start + 1).max(0);
    let candidate_count = first_window_length + second_window_length;
    let get_added_cell_count = |candidate_number: i64| -> i64 {
        if candidate_number <= first_window_length {
            first_offset_window.start() + candidate_number - 1
        } else {
            second_window_start + candidate_number - first_window_length - 1
        }
    };
    let (starting_border_cell, starting_spare_cell_count) = measure_border(split_node);
    let mut measure_candidate = |candidate_number: i64| -> BorderMeasure {
        set_added_cell_count(split_node, get_added_cell_count(candidate_number));
        let (border_cell, spare_cell_count) = measure_border(split_node);
        let moved_cell_count = if is_border_moving_forward {
            border_cell - starting_border_cell
        } else {
            starting_border_cell - border_cell
        };
        BorderMeasure {
            moved_cell_count,
            spare_cell_count,
        }
    };

    // The move stops at the first candidate whose border moved the whole
    // request or whose donor holds no spare cell, else at the last candidate.
    // The border moves at most one cell per candidate, so candidate `n`
    // moved it at most `n` cells.
    let requested_moved_cell_count = i32::from(requested_cell_count);
    let is_move_stopped = |border_measure: BorderMeasure| {
        border_measure.moved_cell_count >= requested_moved_cell_count
            || border_measure.spare_cell_count == 0
    };
    let mut moved_cell_count: u16 = 0;
    let mut kept_added_cell_count: i64 = 0;
    if requested_cell_count > 0 && starting_spare_cell_count > 0 && candidate_count > 0 {
        let probed_candidate_number = i64::from(requested_cell_count).min(candidate_count);
        let probed_measure = measure_candidate(probed_candidate_number);
        let (stopping_candidate_number, stopping_measure) = if is_move_stopped(probed_measure) {
            if probed_measure.spare_cell_count > 0 {
                (probed_candidate_number, probed_measure)
            } else {
                find_first_candidate(
                    1,
                    probed_candidate_number,
                    probed_measure,
                    &mut measure_candidate,
                    is_move_stopped,
                )
            }
        } else if probed_candidate_number == candidate_count {
            (probed_candidate_number, probed_measure)
        } else {
            let last_measure = measure_candidate(candidate_count);
            if is_move_stopped(last_measure) {
                find_first_candidate(
                    probed_candidate_number + 1,
                    candidate_count,
                    last_measure,
                    &mut measure_candidate,
                    is_move_stopped,
                )
            } else {
                (candidate_count, last_measure)
            }
        };
        let stopping_moved_cell_count = stopping_measure.moved_cell_count;
        if stopping_moved_cell_count > 0 {
            let (first_candidate_number, _) = find_first_candidate(
                i64::from(stopping_moved_cell_count),
                stopping_candidate_number,
                stopping_measure,
                &mut measure_candidate,
                |border_measure: BorderMeasure| {
                    border_measure.moved_cell_count >= stopping_moved_cell_count
                },
            );
            moved_cell_count = stopping_moved_cell_count as u16;
            kept_added_cell_count = get_added_cell_count(first_candidate_number);
        }
    }

    if requested_cell_count > 0 && moved_cell_count == 0 {
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
    set_added_cell_count(split_node, kept_added_cell_count);
    Ok((updated_layout_tree, moved_cell_count))
}

/// Where the border of one resize stands at one candidate added count.
#[derive(Clone, Copy)]
struct BorderMeasure {
    /// Cells the border moved from where it started, in the direction the
    /// resize asks for.
    moved_cell_count: i32,
    /// Cells the donor's solved size holds above its floor.
    spare_cell_count: u16,
}

/// The smallest candidate number in
/// `lowest_candidate_number..=highest_candidate_number` whose measure
/// satisfies `is_found`, with that measure. `highest_measure` is the measure
/// of `highest_candidate_number`, and it satisfies `is_found`. Every
/// candidate after the first one that satisfies `is_found` satisfies it too.
/// Each step measures the middle candidate and keeps the half that holds the
/// first match: a range of 200 candidates takes 8 measures.
fn find_first_candidate(
    mut lowest_candidate_number: i64,
    mut highest_candidate_number: i64,
    mut highest_measure: BorderMeasure,
    measure_candidate: &mut impl FnMut(i64) -> BorderMeasure,
    is_found: impl Fn(BorderMeasure) -> bool,
) -> (i64, BorderMeasure) {
    while lowest_candidate_number < highest_candidate_number {
        let middle_candidate_number =
            lowest_candidate_number + (highest_candidate_number - lowest_candidate_number) / 2;
        let middle_measure = measure_candidate(middle_candidate_number);
        if is_found(middle_measure) {
            highest_candidate_number = middle_candidate_number;
            highest_measure = middle_measure;
        } else {
            lowest_candidate_number = middle_candidate_number + 1;
        }
    }
    (highest_candidate_number, highest_measure)
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
