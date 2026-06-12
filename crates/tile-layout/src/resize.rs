//! The resize transaction: move one pane border by an exact cell count.
//!
//! A resize is not a visual adjustment — it permanently shifts cells between
//! two siblings by updating their weights' `resize_delta`, then lets the
//! solver re-derive geometry. Keybindings and mouse border drags both go
//! through this one function, so there is no second geometry-mutation path
//! to drift out of sync.
//!
//! Growing is the only primitive: `resize(pane, Right, n)` moves the pane's
//! right border outward. The same border moved the other way is the
//! neighbor's grow (`resize(neighbor, Left, n)`), so shrink commands need no
//! separate signed form.
//!
//! Panes inside a stack resize as a unit: the stack's outer border is the
//! one that moves, because collapsed children have no independent size.

use thiserror::Error;
use tile_core::error::{DomainCategory, DomainError, Severity};
use tile_core::geometry::{Direction, Rect, SplitDirection};
use tile_core::ids::PaneId;

use crate::solver::{directional_child_rects, slot_floor};
use crate::tree::{LayoutNode, SplitNode};

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
    /// The neighbor on that side cannot give that many cells without going
    /// below its minimum size.
    #[error("resize of {requested} cells exceeds the neighbor's {spare} spare cells")]
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

/// Grow `pane` by `amount` cells toward `direction`, taking them from the
/// adjacent sibling on that side.
///
/// The border that moves is the nearest one: the deepest split along the
/// pane's ancestor chain that runs on the right axis and has a neighbor on
/// that side. `tab_rect` is the rect the tree currently solves into; the
/// neighbor's solved size bounds how much it can give.
///
/// # Errors
///
/// - [`ResizeError::PaneNotFound`] when `pane` is not in the tree.
/// - [`ResizeError::NoAdjacentBorder`] when no ancestor has a neighbor on
///   that side.
/// - [`ResizeError::MinSize`] when the neighbor would drop below its floor.
pub fn resize(
    tree: &LayoutNode,
    tab_rect: Rect,
    pane: PaneId,
    direction: Direction,
    amount: u16,
) -> Result<LayoutNode, ResizeError> {
    let mut path = Vec::new();
    if !path_to(tree, pane, &mut path) {
        return Err(ResizeError::PaneNotFound { pane });
    }

    let horizontal = matches!(direction, Direction::Left | Direction::Right);
    let wanted = if horizontal {
        SplitDirection::Horizontal
    } else {
        SplitDirection::Vertical
    };

    // Deepest ancestor split on the right axis with a neighbor on the
    // resize side — that split owns the border being moved.
    let Some((depth, receiver, donor)) = find_border(tree, &path, wanted, direction) else {
        return Err(ResizeError::NoAdjacentBorder { pane, direction });
    };

    // The donor can give only what its solved size holds above its floor.
    let split = split_at(tree, &path[..depth]);
    let split_rect = rect_at(tree, tab_rect, &path[..depth]);
    let donor_rect = directional_child_rects(split, split_rect)[donor];
    let donor_cells = if horizontal {
        donor_rect.size.cols
    } else {
        donor_rect.size.rows
    };
    let spare = donor_cells.saturating_sub(slot_floor(split, donor, horizontal));
    if amount > spare {
        return Err(ResizeError::MinSize {
            requested: amount,
            spare,
        });
    }

    let mut result = tree.clone();
    let split = split_at_mut(&mut result, &path[..depth]);
    split.weights[receiver].resize_delta = split.weights[receiver]
        .resize_delta
        .saturating_add(i32::from(amount));
    split.weights[donor].resize_delta = split.weights[donor]
        .resize_delta
        .saturating_sub(i32::from(amount));
    Ok(result)
}

/// Record the child index taken at each split from the root down to the
/// leaf holding `pane`. Returns `false` when the pane is not in `node`.
fn path_to(node: &LayoutNode, pane: PaneId, path: &mut Vec<usize>) -> bool {
    match node {
        LayoutNode::Pane(id) => *id == pane,
        LayoutNode::Split(split) => {
            for (index, child) in split.children.iter().enumerate() {
                path.push(index);
                if path_to(&child.node, pane, path) {
                    return true;
                }
                path.pop();
            }
            false
        }
    }
}

/// Find the deepest ancestor split with `wanted` direction where the path's
/// child has a sibling on the `direction` side. Returns the split's depth in
/// the path plus the receiver (the path child) and donor (the sibling).
fn find_border(
    tree: &LayoutNode,
    path: &[usize],
    wanted: SplitDirection,
    direction: Direction,
) -> Option<(usize, usize, usize)> {
    for depth in (0..path.len()).rev() {
        let LayoutNode::Split(split) = node_at(tree, &path[..depth]) else {
            continue;
        };
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

/// The node reached by walking `path` child indices from `tree`.
fn node_at<'tree>(tree: &'tree LayoutNode, path: &[usize]) -> &'tree LayoutNode {
    let mut node = tree;
    for &index in path {
        let LayoutNode::Split(split) = node else {
            unreachable!("path was built over this tree");
        };
        node = &split.children[index].node;
    }
    node
}

/// Like [`node_at`], for paths known to end at a split.
fn split_at<'tree>(tree: &'tree LayoutNode, path: &[usize]) -> &'tree SplitNode {
    match node_at(tree, path) {
        LayoutNode::Split(split) => split,
        LayoutNode::Pane(_) => unreachable!("path was built over this tree"),
    }
}

/// Mutable variant of [`split_at`].
fn split_at_mut<'tree>(tree: &'tree mut LayoutNode, path: &[usize]) -> &'tree mut SplitNode {
    let mut node = tree;
    for &index in path {
        let LayoutNode::Split(split) = node else {
            unreachable!("path was built over this tree");
        };
        node = &mut split.children[index].node;
    }
    match node {
        LayoutNode::Split(split) => split,
        LayoutNode::Pane(_) => unreachable!("path was built over this tree"),
    }
}

/// The rect the node at `path` solves into, starting from `tab_rect`.
///
/// Descends the same geometry the solver derives: directional levels slice
/// with the shared child-rect computation; a stacked level passes its whole
/// rect to the active child and zero to collapsed ones.
fn rect_at(tree: &LayoutNode, tab_rect: Rect, path: &[usize]) -> Rect {
    let mut node = tree;
    let mut rect = tab_rect;
    for &index in path {
        let LayoutNode::Split(split) = node else {
            unreachable!("path was built over this tree");
        };
        rect = match split.direction {
            SplitDirection::Horizontal | SplitDirection::Vertical => {
                directional_child_rects(split, rect)[index]
            }
            SplitDirection::Stacked => {
                if index == split.active {
                    rect
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
