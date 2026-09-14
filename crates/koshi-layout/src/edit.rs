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
use crate::solver::{compute_cell_area, is_content_visible, solve_layout_with_sizing, PaneSizing};
use crate::tree::{compute_split_direction, LayoutNode, SplitNode};

/// A rejected split or stack edit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum SplitError {
    /// The pane to split next to, or to stack onto, is not in this layout.
    #[error("pane {target_pane_id} is not in this layout")]
    PaneNotFound { target_pane_id: PaneId },
}

impl DomainError for SplitError {
    fn category(&self) -> DomainCategory {
        DomainCategory::Layout
    }

    fn get_severity(&self) -> Severity {
        Severity::Recoverable
    }
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
    let operand_depth = (0..pane_path.len())
        .find(|&depth| {
            matches!(
                layout_tree.get_node_at_path(&pane_path[..depth]),
                LayoutNode::Split(split) if split.direction == SplitDirection::Stacked
            )
        })
        .unwrap_or(pane_path.len());

    let mut edited_tree = layout_tree.clone();
    let target_node_slot = edited_tree.get_node_at_path_mut(&pane_path[..operand_depth]);
    let existing_subtree = std::mem::replace(target_node_slot, LayoutNode::Pane(new_pane_id));

    let new_pane_node = LayoutNode::Pane(new_pane_id);
    let children = match direction {
        Direction::Right | Direction::Down => vec![existing_subtree, new_pane_node],
        Direction::Left | Direction::Up => vec![new_pane_node, existing_subtree],
    };
    *target_node_slot = LayoutNode::Split(SplitNode::with_equal_weights(
        compute_split_direction(direction),
        children,
    ));
    Ok(edited_tree)
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
    if !layout_tree.contains_pane(anchor_pane_id) {
        return Err(SplitError::PaneNotFound {
            target_pane_id: anchor_pane_id,
        });
    }

    let mut edited_tree = layout_tree.clone();
    if let Some(stack) = edited_tree.find_containing_stack_mut(anchor_pane_id) {
        stack.children.push(LayoutNode::Pane(new_pane_id));
        stack.weights.push(SizeWeight::default());
        stack.active_child_index = stack.children.len() - 1;
    } else {
        let pane_path = edited_tree
            .find_pane_path(anchor_pane_id)
            .expect("presence checked above");
        let target_node_slot = edited_tree.get_node_at_path_mut(&pane_path);
        *target_node_slot = LayoutNode::Split(SplitNode::from_stacked_pane_ids(
            vec![anchor_pane_id, new_pane_id],
            1,
        ));
    }
    Ok(edited_tree)
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

impl DomainError for RemoveError {
    fn category(&self) -> DomainCategory {
        DomainCategory::Layout
    }

    fn get_severity(&self) -> Severity {
        Severity::Recoverable
    }
}

/// What a removal freed and who took it over. Callers use this to repair
/// focus and to resize the PTYs that grew.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneRemovalOutcome {
    /// The rect the removed pane occupied before removal.
    pub removed_pane_rect: Rect,
    /// Panes whose new rects cover part of `removed_pane_rect`, largest absorbed
    /// area first (ties keep layout order), followed in layout order by
    /// panes that cover none of it but changed size. Removing a stack member
    /// regrows the active member in place without its rect touching the
    /// freed strip; that member is listed in the second group. Zero-area
    /// panes and collapsed stack members (whose rect is their one-row header
    /// strip) are never listed.
    pub absorbing_pane_ids: Vec<PaneId>,
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
/// `tab_rect` is the rect the layout tree solves into; the returned
/// [`PaneRemovalOutcome`] geometry is measured in it. `sizing` is the caller's own
/// [`PaneSizing`]; the before and after solves use it, so they agree with
/// the caller's solve on which panes are suppressed and where each rect
/// sits.
///
/// # Errors
///
/// - [`RemoveError::PaneNotFound`] when `pane_id` has no leaf in `layout_tree`.
/// - [`RemoveError::LastPane`] when `pane_id` is the only pane left, including
///   when the tree's other children are splits holding no leaf.
///
/// The caller's layout tree is unchanged in both cases.
pub fn remove_pane(
    layout_tree: &LayoutNode,
    tab_rect: Rect,
    pane_id: PaneId,
    sizing: PaneSizing,
) -> Result<(LayoutNode, PaneRemovalOutcome), RemoveError> {
    // The solve before the edit gives the rect the pane frees.
    let before_layout = solve_layout_with_sizing(layout_tree, tab_rect, sizing);
    let Some(&(_, removed_pane_rect)) = before_layout
        .pane_rects
        .iter()
        .find(|&&(before_pane_id, _)| before_pane_id == pane_id)
    else {
        return Err(RemoveError::PaneNotFound { pane_id });
    };

    let mut edited_tree = layout_tree.clone();
    match remove_leaf(&mut edited_tree, pane_id) {
        PaneRemovalStatus::PaneNotFound => return Err(RemoveError::PaneNotFound { pane_id }),
        PaneRemovalStatus::SubtreeEmpty => return Err(RemoveError::LastPane { pane_id }),
        PaneRemovalStatus::Removed => {}
    }
    // An empty leaf list means the removed pane was the last leaf, even when
    // empty splits remain.
    if edited_tree.list_leaf_pane_ids().is_empty() {
        return Err(RemoveError::LastPane { pane_id });
    }

    // Solve again after the edit and collect every surviving, visible pane
    // that either grew into the freed space or simply changed size.
    let after_layout = solve_layout_with_sizing(&edited_tree, tab_rect, sizing);
    let mut affected_pane_area_pairs: Vec<(PaneId, u64)> = after_layout
        .pane_rects
        .iter()
        .filter(|&&(pane_id, pane_rect)| {
            is_content_visible(pane_id, pane_rect, &after_layout.stack_headers)
        })
        .filter_map(|&(pane_id, pane_rect)| {
            let absorbed_cell_area = pane_rect
                .compute_intersection(removed_pane_rect)
                .map_or(0, compute_cell_area);
            let is_resized =
                before_layout
                    .pane_rects
                    .iter()
                    .any(|&(before_pane_id, before_pane_rect)| {
                        before_pane_id == pane_id
                            && before_pane_rect.cell_size != pane_rect.cell_size
                    });
            (absorbed_cell_area > 0 || is_resized).then_some((pane_id, absorbed_cell_area))
        })
        .collect();
    // Largest absorbed area first; the stable sort keeps layout order among
    // equal areas, including the zero-overlap resizes.
    affected_pane_area_pairs
        .sort_by_key(|&(_, absorbed_cell_area)| std::cmp::Reverse(absorbed_cell_area));

    Ok((
        edited_tree,
        PaneRemovalOutcome {
            removed_pane_rect,
            absorbing_pane_ids: affected_pane_area_pairs
                .into_iter()
                .map(|(pane_id, _)| pane_id)
                .collect(),
        },
    ))
}

/// What happened below while looking for the leaf to remove.
enum PaneRemovalStatus {
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
fn remove_leaf(layout_node: &mut LayoutNode, pane_id: PaneId) -> PaneRemovalStatus {
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
