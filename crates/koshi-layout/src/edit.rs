//! Structural edits to the layout tree: splitting panes in, stacking them,
//! taking them out.
//!
//! Every edit is pure: it borrows the current tree and returns a new one,
//! leaving the input untouched. A failed edit returns an error and changes
//! nothing; there is no half-edited tree.

use koshi_core::error::{DomainCategory, DomainError, Severity};
use koshi_core::geometry::{Direction, Rect, SplitDirection};
use koshi_core::ids::PaneId;
use thiserror::Error;

use crate::size::SizeWeight;
use crate::solver::{cell_area, shows_content, solve_with_min, PaneSizing};
use crate::tree::{split_axis, LayoutNode, SplitNode};

/// A rejected split or stack edit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum SplitError {
    /// The pane to split next to, or to stack onto, is not in this layout.
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

/// Split next to `target`, placing `new_pane` beside it.
///
/// The operand is replaced by a split of the operand and the new pane with
/// equal weights. `direction` sets the split axis (`Left` and `Right` split
/// horizontally, `Up` and `Down` vertically) and where the new pane lands:
/// `Right` and `Down` put it after the operand, `Left` and `Up` before.
///
/// When `target` sits inside a stack, the operand is the outermost stack on
/// the path to `target`, kept whole. Otherwise the operand is the target's
/// leaf. The parent split keeps the operand's weight for the new split.
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
    let path = tree
        .path_to(target)
        .ok_or(SplitError::PaneNotFound { target })?;
    // The outermost stack on the path is the operand; without one, the leaf
    // itself is.
    let operand_depth = (0..path.len())
        .find(|&depth| {
            matches!(
                tree.node_at(&path[..depth]),
                LayoutNode::Split(split) if split.direction == SplitDirection::Stacked
            )
        })
        .unwrap_or(path.len());

    let mut result = tree.clone();
    let slot = result.node_at_mut(&path[..operand_depth]);
    let operand = std::mem::replace(slot, LayoutNode::Pane(new_pane));

    let old = operand;
    let new = LayoutNode::Pane(new_pane);
    let children = match direction {
        Direction::Right | Direction::Down => vec![old, new],
        Direction::Left | Direction::Up => vec![new, old],
    };
    *slot = LayoutNode::Split(SplitNode::with_equal_weights(
        split_axis(direction),
        children,
    ));
    Ok(result)
}

/// Stack `new_pane` onto `anchor`'s position.
///
/// If `anchor` already sits inside a stack, the new pane is appended as the
/// last member of the innermost stack holding it; otherwise the anchor's
/// leaf becomes a two-member stack of `anchor` then `new_pane`. Either way
/// the new pane is the active (expanded) member afterwards and every other
/// member is collapsed.
///
/// # Errors
///
/// [`SplitError::PaneNotFound`] when `anchor` has no leaf in `tree`; the
/// caller's tree is unchanged.
pub fn add_to_stack(
    tree: &LayoutNode,
    anchor: PaneId,
    new_pane: PaneId,
) -> Result<LayoutNode, SplitError> {
    if !tree.contains_pane(anchor) {
        return Err(SplitError::PaneNotFound { target: anchor });
    }

    let mut result = tree.clone();
    if let Some(stack) = result.stack_containing_mut(anchor) {
        stack.children.push(LayoutNode::Pane(new_pane));
        stack.weights.push(SizeWeight::default());
        stack.active = stack.children.len() - 1;
    } else {
        let path = result.path_to(anchor).expect("presence checked above");
        let slot = result.node_at_mut(&path);
        *slot = LayoutNode::Split(SplitNode::stack(vec![anchor, new_pane], 1));
    }
    Ok(result)
}

/// A rejected removal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum RemoveError {
    /// The pane to remove is not in this layout.
    #[error("pane {pane} is not in this layout")]
    PaneNotFound { pane: PaneId },
    /// The pane to remove is the only pane in this layout; removing it would
    /// leave no layout at all.
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
    /// area first (ties keep layout order), followed in layout order by
    /// panes that cover none of it but changed size. Removing a stack member
    /// regrows the active member in place without its rect touching the
    /// freed strip; that member is listed in the second group. Zero-area
    /// panes and collapsed stack members (whose rect is their one-row header
    /// strip) are never listed.
    pub absorbed_by: Vec<PaneId>,
}

/// Remove the leaf holding `pane`; its space flows to the siblings on the
/// next solve.
///
/// Splits emptied by the removal are pruned. A split left with a single
/// child is kept; normalization is a separate, explicit step. Inside a
/// stack, removal keeps exactly one child expanded: removing the active
/// member activates the one that slides into its place, removing any other
/// member leaves the active one alone. A stack left with one member stays a
/// one-member stack; a stack left with none is pruned like any emptied
/// split.
///
/// `tab_rect` is the rect the tree solves into; the returned
/// [`RemovalInfo`] geometry is measured in it. `sizing` is the caller's own
/// [`PaneSizing`]; the before and after solves use it, so they agree with
/// the caller's solve on which panes are suppressed and where each rect
/// sits.
///
/// # Errors
///
/// - [`RemoveError::PaneNotFound`] when `pane` has no leaf in `tree`.
/// - [`RemoveError::LastPane`] when `pane` is the only pane left, including
///   when the tree's other children are splits holding no leaf.
///
/// The caller's tree is unchanged in both cases.
pub fn remove_pane(
    tree: &LayoutNode,
    tab_rect: Rect,
    pane: PaneId,
    sizing: PaneSizing,
) -> Result<(LayoutNode, RemovalInfo), RemoveError> {
    // The solve before the edit gives the rect the pane frees.
    let before = solve_with_min(tree, tab_rect, sizing);
    let Some(&(_, old_rect)) = before.panes.iter().find(|&&(id, _)| id == pane) else {
        return Err(RemoveError::PaneNotFound { pane });
    };

    let mut result = tree.clone();
    match remove_leaf(&mut result, pane) {
        Removal::NotHere => return Err(RemoveError::PaneNotFound { pane }),
        Removal::NodeEmptied => return Err(RemoveError::LastPane { pane }),
        Removal::Done => {}
    }
    // A tree whose only remaining children are empty splits holds no pane, so
    // `pane` was the last one however many nodes survive.
    if result.leaf_panes().is_empty() {
        return Err(RemoveError::LastPane { pane });
    }

    // Solve again after the edit and collect every surviving, visible pane
    // that either grew into the freed space or simply changed size.
    let after = solve_with_min(&result, tab_rect, sizing);
    let mut absorbers: Vec<(PaneId, u64)> = after
        .panes
        .iter()
        .filter(|&&(id, rect)| shows_content(id, rect, &after.stack_headers))
        .filter_map(|&(id, rect)| {
            let overlap = rect.intersection(old_rect).map_or(0, cell_area);
            let resized = before
                .panes
                .iter()
                .any(|&(before_id, before_rect)| before_id == id && before_rect.size != rect.size);
            (overlap > 0 || resized).then_some((id, overlap))
        })
        .collect();
    // Largest absorbed area first; the stable sort keeps layout order among
    // equal areas, including the zero-overlap resizes.
    absorbers.sort_by_key(|&(_, area)| std::cmp::Reverse(area));

    Ok((
        result,
        RemovalInfo {
            old_rect,
            absorbed_by: absorbers.into_iter().map(|(id, _)| id).collect(),
        },
    ))
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

/// Walks `node` depth-first for the leaf holding `pane` and drops it, along
/// with every split the drop empties below `node`. Returns
/// [`Removal::NodeEmptied`] when `node` itself is left with no child.
fn remove_leaf(node: &mut LayoutNode, pane: PaneId) -> Removal {
    let LayoutNode::Split(split) = node else {
        return if *node == LayoutNode::Pane(pane) {
            Removal::NodeEmptied
        } else {
            Removal::NotHere
        };
    };

    for index in 0..split.children.len() {
        match remove_leaf(&mut split.children[index], pane) {
            Removal::NotHere => continue,
            Removal::Done => return Removal::Done,
            Removal::NodeEmptied => {
                // That child's subtree lost its last pane: drop the child
                // and its weight, then repair this split's active slot.
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

/// Keep `active` pointing at the same child after the child at
/// `removed_index` is gone, clamped into bounds: removing the active child
/// activates the one that slid into its place.
fn reseat_active(split: &mut SplitNode, removed_index: usize) {
    if removed_index < split.active {
        split.active -= 1;
    }
    split.active = split.active_index();
}

#[cfg(test)]
mod tests;
