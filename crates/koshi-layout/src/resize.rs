//! The resize transaction: move one pane border by an exact cell count.
//!
//! A resize permanently shifts cells between two siblings by updating their
//! weights' `resize_delta`, then lets the solver re-derive geometry.
//!
//! The size is signed and names the border by direction: `resize(pane,
//! Right, 5)` moves the pane's right border outward (the pane grows,
//! the right neighbor donates), and `resize(pane, Right, -5)` moves the
//! same border inward (the pane donates, the right neighbor gains).
//!
//! Panes inside a stack resize as a unit: the border that moves is the
//! stack's outer one, never a border between two stack members.

use koshi_core::error::{DomainCategory, DomainError, Severity};
use koshi_core::geometry::{Direction, Point, Rect, Size, SplitDirection};
use koshi_core::ids::PaneId;
use thiserror::Error;

use crate::size::SizeWeight;
use crate::solver::{directional_child_rects, slot_floor, stack_min_size, PaneSizing};
use crate::tree::{split_axis, LayoutNode};

/// A rejected resize. The caller's tree is unchanged in every case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ResizeError {
    /// The pane to resize is not in this layout.
    #[error("pane {pane} is not in this layout")]
    PaneNotFound { pane: PaneId },
    /// No border exists on that side: the pane touches the tab edge there
    /// at every level of the tree.
    #[error("pane {pane} has no {direction:?} border to adjust")]
    NoAdjacentBorder { pane: PaneId, direction: Direction },
    /// The pane giving up the cells — the neighbor on a grow, the pane
    /// itself on a shrink — cannot give that many without going below its
    /// minimum size.
    #[error("resize of {requested} cells exceeds the donating pane's {spare} spare cells")]
    MinSize { requested: u16, spare: u16 },
}

impl DomainError for ResizeError {
    fn category(&self) -> DomainCategory {
        DomainCategory::Layout
    }

    fn severity(&self) -> Severity {
        Severity::Recoverable
    }
}

/// Move `pane`'s border on the `direction` side by `size` cells: positive
/// moves it outward (the pane grows and the adjacent sibling on that side
/// donates the cells), negative moves it inward (the pane donates and that
/// sibling gains them). A `size` of `0` runs the same lookups and checks
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
/// - [`ResizeError::PaneNotFound`] when `pane` is not in the tree.
/// - [`ResizeError::NoAdjacentBorder`] when no ancestor has a neighbor on
///   that side.
/// - [`ResizeError::MinSize`] when the donating side would drop below its
///   floor.
pub fn resize(
    tree: &LayoutNode,
    tab_rect: Rect,
    pane: PaneId,
    direction: Direction,
    size: i16,
) -> Result<LayoutNode, ResizeError> {
    resize_with_min(tree, tab_rect, pane, direction, size, PaneSizing::default())
}

/// Like [`resize`] with an explicit [`PaneSizing`]: `sizing.min` is the
/// per-pane content floor the donor's spare is measured against, and the
/// donor's solved size excludes the [`PaneSizing::gap`] beside it.
/// [`resize`] passes [`PaneSizing::default`].
pub fn resize_with_min(
    tree: &LayoutNode,
    tab_rect: Rect,
    pane: PaneId,
    direction: Direction,
    size: i16,
    sizing: PaneSizing,
) -> Result<LayoutNode, ResizeError> {
    let path = tree
        .path_to(pane)
        .ok_or(ResizeError::PaneNotFound { pane })?;

    let wanted = split_axis(direction);
    let horizontal = wanted == SplitDirection::Horizontal;

    // The deepest ancestor split on the wanted axis with a neighbor on the
    // resize side owns the border being moved.
    let (depth, pane_slot, neighbor) = find_border(tree, &path, wanted, direction)
        .ok_or(ResizeError::NoAdjacentBorder { pane, direction })?;

    // The sign picks who donates the cells across the border: on a grow the
    // neighbor gives them to the pane, on a shrink the pane gives them to
    // the neighbor.
    let amount = size.unsigned_abs();
    let (receiver, donor) = if size < 0 {
        (neighbor, pane_slot)
    } else {
        (pane_slot, neighbor)
    };

    // The donor can give only what its solved size holds above its floor.
    let split = tree.split_at(&path[..depth]);
    let split_rect = rect_at(tree, tab_rect, &path[..depth], sizing);
    let donor_rect = directional_child_rects(split, split_rect, sizing)[donor];
    let donor_cells = if horizontal {
        donor_rect.size.cols
    } else {
        donor_rect.size.rows
    };
    let spare = donor_cells.saturating_sub(slot_floor(split, donor, horizontal, sizing));
    if amount > spare {
        return Err(ResizeError::MinSize {
            requested: amount,
            spare,
        });
    }

    let mut result = tree.clone();
    let split = result.split_at_mut(&path[..depth]);
    // Missing weights are padded with the default share up to the child
    // count.
    if split.weights.len() < split.children.len() {
        split
            .weights
            .resize(split.children.len(), SizeWeight::default());
    }
    split.weights[receiver].resize_delta = split.weights[receiver]
        .resize_delta
        .saturating_add(i32::from(amount));
    split.weights[donor].resize_delta = split.weights[donor]
        .resize_delta
        .saturating_sub(i32::from(amount));
    Ok(result)
}

/// `true` when [`resize`] on `pane` toward `direction` finds a border to
/// move: the pane is in the tree and an ancestor split on the matching axis,
/// above any collapsed stack member, has a sibling on that side. `false`
/// for a pane not in the tree, for a side on the tab edge, and for the
/// boundary against a collapsed stack header.
#[must_use]
pub fn has_adjacent_border(tree: &LayoutNode, pane: PaneId, direction: Direction) -> bool {
    let Some(path) = tree.path_to(pane) else {
        return false;
    };
    find_border(tree, &path, split_axis(direction), direction).is_some()
}

/// The deepest ancestor split of direction `wanted`, above any collapsed
/// stack member on `path`, whose path child has a sibling on the `direction`
/// side: its depth in `path`, the path child's index, and the sibling's
/// index. `None` when no such split exists.
fn find_border(
    tree: &LayoutNode,
    path: &[usize],
    wanted: SplitDirection,
    direction: Direction,
) -> Option<(usize, usize, usize)> {
    // Only splits above the first stacked split whose path child is
    // collapsed are candidates.
    let mut visible = path.len();
    let mut node = tree;
    for (depth, &index) in path.iter().enumerate() {
        let LayoutNode::Split(split) = node else {
            break;
        };
        if split.direction == SplitDirection::Stacked && index != split.active_index() {
            visible = depth;
            break;
        }
        node = &split.children[index].node;
    }

    for depth in (0..visible).rev() {
        let split = tree.split_at(&path[..depth]);
        if split.direction != wanted {
            continue;
        }
        let receiver = path[depth];
        let donor = match direction {
            Direction::Left | Direction::Up => receiver.checked_sub(1),
            Direction::Right | Direction::Down => {
                (receiver + 1 < split.children.len()).then_some(receiver + 1)
            }
        };
        if let Some(donor) = donor {
            return Some((depth, receiver, donor));
        }
    }
    None
}

/// The rect the node at `path` solves into, starting from `tab_rect`.
///
/// A directional level takes the child rect [`directional_child_rects`]
/// derives. A stacked level gives the active member the stack rect minus
/// one header row per other member, shifted down by the headers above it;
/// a collapsed member, or every member of a stack whose rect is smaller
/// than [`stack_min_size`], gets a zero rect.
fn rect_at(tree: &LayoutNode, tab_rect: Rect, path: &[usize], sizing: PaneSizing) -> Rect {
    let mut node = tree;
    let mut rect = tab_rect;
    for &index in path {
        let LayoutNode::Split(split) = node else {
            unreachable!("path was built over this tree");
        };
        rect = match split.direction {
            SplitDirection::Horizontal | SplitDirection::Vertical => {
                directional_child_rects(split, rect, sizing)[index]
            }
            SplitDirection::Stacked => {
                let needed = stack_min_size(split, sizing);
                if rect.size.rows < needed.rows || rect.size.cols < needed.cols {
                    Rect::zero()
                } else if index == split.active_index() {
                    let header_rows = split.children.len().saturating_sub(1) as u16;
                    Rect::new(
                        Point {
                            x: rect.origin.x,
                            y: rect.origin.y.saturating_add(index as u16),
                        },
                        Size {
                            cols: rect.size.cols,
                            rows: rect.size.rows.saturating_sub(header_rows),
                        },
                    )
                } else {
                    Rect::zero()
                }
            }
        };
        node = &split.children[index].node;
    }
    rect
}

#[cfg(test)]
mod tests;
