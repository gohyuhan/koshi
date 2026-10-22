//! Structural edits to the layout tree: splitting panes in, stacking them,
//! taking them out.
//!
//! Every edit is pure: it borrows the current tree and returns a new one,
//! leaving the input untouched. A failed edit returns an error and changes
//! nothing; there is no half-edited tree.

use koshi_core::geometry::{Direction, SplitDirection};
use koshi_core::ids::PaneId;
use thiserror::Error;

use crate::size::SizeWeight;
use crate::tree::{compute_split_direction, LayoutNode, SplitNode};

/// A rejected split or stack edit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum SplitError {
    /// The pane to split next to, or to stack onto, is not in this layout.
    #[error("pane {target_pane_id} is not in this layout")]
    PaneNotFound { target_pane_id: PaneId },
}

/// Split next to `target_pane_id`, placing `new_pane_id` beside it.
///
/// The operand is replaced by a split of the operand and the new pane with
/// equal weights. `direction` sets the split axis (`Left` and `Right` split
/// horizontally, `Up` and `Down` vertically) and where the new pane lands:
/// `Right` and `Down` put it after the operand, `Left` and `Up` before.
///
/// When `target_pane_id` sits inside a stack, the operand is the outermost stack on
/// the path to `target_pane_id`, kept whole. Otherwise the operand is the target's
/// leaf. The parent split keeps the operand's weight for the new split.
///
/// # Errors
///
/// [`SplitError::PaneNotFound`] when `target_pane_id` has no leaf in `layout_tree`; the
/// caller's tree is unchanged.
pub fn split_leaf(
    layout_tree: &LayoutNode,
    target_pane_id: PaneId,
    new_pane_id: PaneId,
    direction: Direction,
) -> Result<LayoutNode, SplitError> {
    let pane_path = layout_tree
        .find_pane_path(target_pane_id)
        .ok_or(SplitError::PaneNotFound { target_pane_id })?;
    // Select the outermost stacked ancestor, or the target leaf when none exists.
    let split_operand_depth = (0..pane_path.len())
        .find(|&pane_path_depth| {
            matches!(
                layout_tree.get_node_at_path(&pane_path[..pane_path_depth]),
                LayoutNode::Split(split_node) if split_node.direction == SplitDirection::Stacked
            )
        })
        .unwrap_or(pane_path.len());

    let mut edited_layout_tree = layout_tree.clone();
    let split_operand_node_slot =
        edited_layout_tree.get_node_at_path_mut(&pane_path[..split_operand_depth]);
    let existing_pane_subtree =
        std::mem::replace(split_operand_node_slot, LayoutNode::Pane(new_pane_id));

    let new_pane_node = LayoutNode::Pane(new_pane_id);
    let split_children = match direction {
        Direction::Right | Direction::Down => vec![existing_pane_subtree, new_pane_node],
        Direction::Left | Direction::Up => vec![new_pane_node, existing_pane_subtree],
    };
    *split_operand_node_slot = LayoutNode::Split(SplitNode::with_equal_weights(
        compute_split_direction(direction),
        split_children,
    ));
    Ok(edited_layout_tree)
}

/// Stack `new_pane_id` onto `anchor_pane_id`'s position.
///
/// If `anchor_pane_id` already sits inside a stack, the new pane is appended as the
/// last member of the innermost stack holding it; otherwise the anchor's
/// leaf becomes a two-member stack of `anchor_pane_id` then `new_pane_id`. Either way
/// the new pane is the active (expanded) member afterwards and every other
/// member is collapsed.
///
/// # Errors
///
/// [`SplitError::PaneNotFound`] when `anchor_pane_id` has no leaf in `layout_tree`; the
/// caller's tree is unchanged.
pub fn add_pane_to_stack(
    layout_tree: &LayoutNode,
    anchor_pane_id: PaneId,
    new_pane_id: PaneId,
) -> Result<LayoutNode, SplitError> {
    if !layout_tree.has_pane(anchor_pane_id) {
        return Err(SplitError::PaneNotFound {
            target_pane_id: anchor_pane_id,
        });
    }

    let mut edited_layout_tree = layout_tree.clone();
    if let Some(containing_stack) = edited_layout_tree.find_containing_stack_mut(anchor_pane_id) {
        containing_stack
            .children
            .push(LayoutNode::Pane(new_pane_id));
        containing_stack.weights.push(SizeWeight::default());
        containing_stack.active_child_index = containing_stack.children.len() - 1;
    } else {
        let pane_path = edited_layout_tree
            .find_pane_path(anchor_pane_id)
            .expect("presence checked above");
        let anchor_pane_node_slot = edited_layout_tree.get_node_at_path_mut(&pane_path);
        *anchor_pane_node_slot = LayoutNode::Split(SplitNode::from_stacked_pane_ids(
            vec![anchor_pane_id, new_pane_id],
            1,
        ));
    }
    Ok(edited_layout_tree)
}

/// A rejected removal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum RemoveError {
    /// The pane to remove is not in this layout.
    #[error("pane {pane_id} is not in this layout")]
    PaneNotFound { pane_id: PaneId },
    /// The pane to remove is the only pane in this layout; removing it would
    /// leave no layout at all.
    #[error("pane {pane_id} is the last pane in this layout")]
    LastPane { pane_id: PaneId },
}

/// Remove the leaf holding `pane_id`; its space flows to the siblings on the
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
/// # Errors
///
/// - [`RemoveError::PaneNotFound`] when `pane_id` has no leaf in `layout_tree`.
/// - [`RemoveError::LastPane`] when `pane_id` is the only pane left, including
///   when the tree's other children are splits holding no leaf.
///
/// The caller's layout tree is unchanged in both cases.
pub fn remove_pane(layout_tree: &LayoutNode, pane_id: PaneId) -> Result<LayoutNode, RemoveError> {
    let mut edited_layout_tree = layout_tree.clone();
    match remove_leaf(&mut edited_layout_tree, pane_id) {
        PaneRemovalStatus::PaneNotFound => return Err(RemoveError::PaneNotFound { pane_id }),
        PaneRemovalStatus::SubtreeEmpty => return Err(RemoveError::LastPane { pane_id }),
        PaneRemovalStatus::Removed => {}
    }
    // An empty leaf list means the removed pane was the last leaf, even when
    // empty splits remain.
    if edited_layout_tree.list_leaf_pane_ids().is_empty() {
        return Err(RemoveError::LastPane { pane_id });
    }
    Ok(edited_layout_tree)
}

/// What happened below while looking for the leaf to remove.
pub(crate) enum PaneRemovalStatus {
    /// The pane is not in this subtree.
    PaneNotFound,
    /// Removed; the subtree is still alive.
    Removed,
    /// Removed, and this whole node is now empty — the parent must drop it.
    SubtreeEmpty,
}

/// Walks `layout_node` depth-first for the leaf holding `pane_id` and drops it, along
/// with every split the drop empties below `layout_node`. Returns
/// [`PaneRemovalStatus::SubtreeEmpty`] when `layout_node` itself is left with no child.
pub(crate) fn remove_leaf(layout_node: &mut LayoutNode, pane_id: PaneId) -> PaneRemovalStatus {
    let LayoutNode::Split(split_node) = layout_node else {
        return if *layout_node == LayoutNode::Pane(pane_id) {
            PaneRemovalStatus::SubtreeEmpty
        } else {
            PaneRemovalStatus::PaneNotFound
        };
    };

    for child_index in 0..split_node.children.len() {
        match remove_leaf(&mut split_node.children[child_index], pane_id) {
            PaneRemovalStatus::PaneNotFound => continue,
            PaneRemovalStatus::Removed => return PaneRemovalStatus::Removed,
            PaneRemovalStatus::SubtreeEmpty => {
                // That child's subtree lost its last pane: drop the child
                // and its weight, then repair this split's active slot.
                split_node.children.remove(child_index);
                if child_index < split_node.weights.len() {
                    split_node.weights.remove(child_index);
                }
                if split_node.children.is_empty() {
                    return PaneRemovalStatus::SubtreeEmpty;
                }
                set_active_child_after_removal(split_node, child_index);
                return PaneRemovalStatus::Removed;
            }
        }
    }
    PaneRemovalStatus::PaneNotFound
}

/// Keep the active child index pointing at the same child after the child at
/// `removed_child_index` is gone, clamped into bounds: removing the active child
/// activates the one that slid into its place.
fn set_active_child_after_removal(split_node: &mut SplitNode, removed_child_index: usize) {
    if removed_child_index < split_node.active_child_index {
        split_node.active_child_index -= 1;
    }
    split_node.active_child_index = split_node.get_active_child_index();
}

#[cfg(test)]
mod tests;
