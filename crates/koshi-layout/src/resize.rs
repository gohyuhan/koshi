//! The resize transaction: move one pane border by an exact cell count.
//!
//! A resize is not a visual adjustment — it permanently shifts cells between
//! two siblings by updating their weights' `resize_delta`, then lets the
//! solver re-derive geometry. Keybindings and mouse border drags both go
//! through this one function, so there is no second geometry-mutation path
//! to drift out of sync.
//!
//! The size is signed and names the border by direction: `resize(pane,
//! Right, 5)` moves the pane's right border outward (the pane grows,
//! the right neighbor donates), and `resize(pane, Right, -5)` moves the
//! same border inward (the pane donates, the right neighbor gains).
//!
//! Panes inside a stack resize as a unit: the stack's outer border is the
//! one that moves, because collapsed children have no independent size.

use koshi_core::error::{DomainCategory, DomainError, Severity};
use koshi_core::geometry::{Direction, Point, Rect, Size, SplitDirection};
use koshi_core::ids::PaneId;
use thiserror::Error;

use crate::size::SizeWeight;
use crate::solver::{directional_child_rects, slot_floor};
use crate::tree::LayoutNode;

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
/// sibling gains them). Zero moves nothing and returns the tree unchanged.
///
/// The border that moves is the nearest one to the pane: walking up the
/// pane's ancestors, the first split that runs on the matching axis
/// (horizontal for left/right, vertical for up/down) *and* has a sibling on
/// the `direction` side owns it. Walking upward is what makes nested
/// layouts behave: if the pane touches its inner split's edge, the border
/// that actually moves is the enclosing split's — exactly the line the
/// user sees next to the pane. `tab_rect` is the rect the tree currently
/// solves into; the donating side's solved size bounds how much it can give.
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
    let Some(path) = tree.path_to(pane) else {
        return Err(ResizeError::PaneNotFound { pane });
    };

    let horizontal = matches!(direction, Direction::Left | Direction::Right);
    let wanted = if horizontal {
        SplitDirection::Horizontal
    } else {
        SplitDirection::Vertical
    };

    // Deepest ancestor split on the right axis with a neighbor on the
    // resize side — that split owns the border being moved.
    let Some((depth, pane_slot, neighbor)) = find_border(tree, &path, wanted, direction) else {
        return Err(ResizeError::NoAdjacentBorder { pane, direction });
    };

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
    let split = result.split_at_mut(&path[..depth]);
    // A deserialized split may carry fewer weights than children (stale format);
    // pad each missing weight with the default share — the normalization repair
    // that realigns weights with children — before indexing into them.
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

/// Find the deepest ancestor split with `wanted` direction where the path's
/// child has a sibling on the `direction` side. Returns the split's depth in
/// the path plus the receiver (the path child) and donor (the sibling).
fn find_border(
    tree: &LayoutNode,
    path: &[usize],
    wanted: SplitDirection,
    direction: Direction,
) -> Option<(usize, usize, usize)> {
    // Splits inside a collapsed stack member are invisible — its panes
    // resize the stack as a unit, so only borders above the first collapsed
    // crossing are candidates and the search bubbles to the outer levels.
    let mut visible = path.len();
    let mut node = tree;
    for (depth, &index) in path.iter().enumerate() {
        let LayoutNode::Split(split) = node else {
            break;
        };
        if split.direction == SplitDirection::Stacked {
            let active = split.active.min(split.children.len().saturating_sub(1));
            if index != active {
                visible = depth;
                break;
            }
        }
        node = &split.children[index].node;
    }

    for depth in (0..visible).rev() {
        let LayoutNode::Split(split) = tree.node_at(&path[..depth]) else {
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

/// The rect the node at `path` solves into, starting from `tab_rect`.
///
/// Descends the same geometry the solver derives: directional levels slice
/// with the shared child-rect computation; a stacked level carves one
/// header row per collapsed member out of the active child's rect, shifted
/// below the headers above it, and passes zero to collapsed ones.
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
                // Mirror `solve_stacked`: same clamp on the active index,
                // one header row per other member carved out of the active
                // rect, headers above the active member shifting it down.
                let active = split.active.min(split.children.len().saturating_sub(1));
                if index == active {
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
