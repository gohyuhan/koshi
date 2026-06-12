//! Structural edits to the layout tree: splitting panes in, taking them out,
//! swapping their contents.
//!
//! Every edit is pure: it borrows the current tree and returns a new one,
//! leaving the input untouched. A failed edit returns an error and changes
//! nothing, so callers can validate-and-apply in one step — there is no
//! half-edited tree to roll back.

use thiserror::Error;
use tile_core::error::{DomainCategory, DomainError, Severity};
use tile_core::geometry::{Direction, Rect, SplitDirection};
use tile_core::ids::PaneId;

use crate::solver::solve;
use crate::tree::{LayoutChild, LayoutNode, SplitNode};

/// A rejected split.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum SplitError {
    /// The pane to split next to is not in this layout.
    #[error("pane {target} is not in this layout")]
    PaneNotFound { target: PaneId },
}

impl DomainError for SplitError {
    fn category(&self) -> DomainCategory {
        DomainCategory::Layout
    }

    fn severity(&self) -> Severity {
        Severity::Recoverable
    }
}

/// Split the leaf holding `target`, placing `new_pane` beside it.
///
/// The leaf is replaced by a directional split of the two panes with equal
/// weights. `direction` is where the new pane lands relative to the target:
/// `Right` and `Down` put it after, `Left` and `Up` before.
///
/// # Errors
///
/// [`SplitError::PaneNotFound`] when `target` has no leaf in `tree`; the
/// caller's tree is unchanged.
pub fn split_leaf(
    tree: &LayoutNode,
    target: PaneId,
    new_pane: PaneId,
    direction: Direction,
) -> Result<LayoutNode, SplitError> {
    let mut result = tree.clone();
    let Some(slot) = find_leaf_mut(&mut result, target) else {
        return Err(SplitError::PaneNotFound { target });
    };

    let split_direction = match direction {
        Direction::Left | Direction::Right => SplitDirection::Horizontal,
        Direction::Up | Direction::Down => SplitDirection::Vertical,
    };
    let old = LayoutChild::new(LayoutNode::Pane(target));
    let new = LayoutChild::new(LayoutNode::Pane(new_pane));
    let children = match direction {
        Direction::Right | Direction::Down => vec![old, new],
        Direction::Left | Direction::Up => vec![new, old],
    };
    *slot = LayoutNode::Split(SplitNode::with_equal_weights(split_direction, children));
    Ok(result)
}

/// A rejected removal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum RemoveError {
    /// The pane to remove is not in this layout.
    #[error("pane {pane} is not in this layout")]
    PaneNotFound { pane: PaneId },
    /// Removing the only remaining pane would leave no layout at all;
    /// callers close the tab instead.
    #[error("pane {pane} is the last pane in this layout")]
    LastPane { pane: PaneId },
}

impl DomainError for RemoveError {
    fn category(&self) -> DomainCategory {
        DomainCategory::Layout
    }

    fn severity(&self) -> Severity {
        Severity::Recoverable
    }
}

/// What a removal freed and who took it over. Callers use this to repair
/// focus and to resize the PTYs that grew.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemovalInfo {
    /// The rect the removed pane occupied before removal.
    pub old_rect: Rect,
    /// Panes whose new rects cover part of `old_rect`, largest absorbed
    /// area first (ties keep layout order).
    pub absorbed_by: Vec<PaneId>,
}

/// Remove the leaf holding `pane`; its space flows to the siblings on the
/// next solve.
///
/// Splits emptied by the removal are pruned so no region of the tab goes
/// dead, but a split left with a single child is kept — normalization is a
/// separate, explicit step. Inside a stack, removal keeps exactly one child
/// expanded: removing the active child activates the next one.
///
/// `tab_rect` is the rect the tree currently solves into; it anchors the
/// returned [`RemovalInfo`] geometry.
///
/// # Errors
///
/// - [`RemoveError::PaneNotFound`] when `pane` has no leaf in `tree`.
/// - [`RemoveError::LastPane`] when `pane` is the only pane left.
///
/// The caller's tree is unchanged in both cases.
pub fn remove_pane(
    tree: &LayoutNode,
    tab_rect: Rect,
    pane: PaneId,
) -> Result<(LayoutNode, RemovalInfo), RemoveError> {
    let before = solve(tree, tab_rect);
    let Some(&(_, old_rect)) = before.panes.iter().find(|&&(id, _)| id == pane) else {
        return Err(RemoveError::PaneNotFound { pane });
    };

    let mut result = tree.clone();
    match remove_leaf(&mut result, pane) {
        Removal::NotHere => return Err(RemoveError::PaneNotFound { pane }),
        Removal::NodeEmptied => return Err(RemoveError::LastPane { pane }),
        Removal::Done => {}
    }

    let after = solve(&result, tab_rect);
    let mut absorbers: Vec<(PaneId, u64)> = after
        .panes
        .iter()
        .filter_map(|&(id, rect)| {
            rect.intersection(old_rect)
                .map(|overlap| (id, cell_area(overlap)))
        })
        .collect();
    absorbers.sort_by_key(|&(_, area)| std::cmp::Reverse(area));

    Ok((
        result,
        RemovalInfo {
            old_rect,
            absorbed_by: absorbers.into_iter().map(|(id, _)| id).collect(),
        },
    ))
}

fn cell_area(rect: Rect) -> u64 {
    u64::from(rect.size.cols) * u64::from(rect.size.rows)
}

/// What happened below while looking for the leaf to remove.
enum Removal {
    /// The pane is not in this subtree.
    NotHere,
    /// Removed; the subtree is still alive.
    Done,
    /// Removed, and this whole node is now empty — the parent must drop it.
    NodeEmptied,
}

fn remove_leaf(node: &mut LayoutNode, pane: PaneId) -> Removal {
    let LayoutNode::Split(split) = node else {
        return if matches!(node, LayoutNode::Pane(id) if *id == pane) {
            Removal::NodeEmptied
        } else {
            Removal::NotHere
        };
    };

    for index in 0..split.children.len() {
        match remove_leaf(&mut split.children[index].node, pane) {
            Removal::NotHere => continue,
            Removal::Done => return Removal::Done,
            Removal::NodeEmptied => {
                split.children.remove(index);
                if index < split.weights.len() {
                    split.weights.remove(index);
                }
                if split.children.is_empty() {
                    return Removal::NodeEmptied;
                }
                reseat_active(split, index);
                return Removal::Done;
            }
        }
    }
    Removal::NotHere
}

/// Keep `active` pointing at the right child after removing `removed_index`,
/// and keep a stack's "exactly one expanded child" shape intact: removing
/// the active child activates the one that slid into its place.
fn reseat_active(split: &mut SplitNode, removed_index: usize) {
    if removed_index < split.active {
        split.active -= 1;
    }
    split.active = split.active.min(split.children.len() - 1);
    if split.direction == SplitDirection::Stacked {
        for (index, child) in split.children.iter_mut().enumerate() {
            child.collapsed = index != split.active;
        }
    }
}

/// The mutable slot of the leaf holding `target`, if it exists.
fn find_leaf_mut(node: &mut LayoutNode, target: PaneId) -> Option<&mut LayoutNode> {
    if matches!(node, LayoutNode::Pane(id) if *id == target) {
        return Some(node);
    }
    if let LayoutNode::Split(split) = node {
        for child in &mut split.children {
            if let Some(found) = find_leaf_mut(&mut child.node, target) {
                return Some(found);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests;
