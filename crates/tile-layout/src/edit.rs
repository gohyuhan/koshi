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

use crate::size::SizeWeight;
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

/// Split next to `target`, placing `new_pane` beside it.
///
/// The operand is replaced by a directional split of operand and new pane
/// with equal weights. `direction` is where the new pane lands: `Right` and
/// `Down` put it after, `Left` and `Up` before.
///
/// When `target` sits inside a stack, the operand is the whole stack — a
/// directional split never breaks a stack open, it places the new pane
/// beside it. Otherwise the operand is the target's leaf.
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
    let Some(path) = tree.path_to(target) else {
        return Err(SplitError::PaneNotFound { target });
    };
    // The outermost stack on the path owns the split; without one, the
    // leaf itself does.
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

    let split_direction = match direction {
        Direction::Left | Direction::Right => SplitDirection::Horizontal,
        Direction::Up | Direction::Down => SplitDirection::Vertical,
    };
    let old = LayoutChild::new(operand);
    let new = LayoutChild::new(LayoutNode::Pane(new_pane));
    let children = match direction {
        Direction::Right | Direction::Down => vec![old, new],
        Direction::Left | Direction::Up => vec![new, old],
    };
    *slot = LayoutNode::Split(SplitNode::with_equal_weights(split_direction, children));
    Ok(result)
}

/// Stack `new_pane` onto `anchor`'s position.
///
/// If `anchor` already sits inside a stack, the new pane joins that stack;
/// otherwise the anchor's leaf becomes a two-member stack. Either way the
/// new pane is the active (expanded) member afterwards, matching how a
/// directional split focuses the new pane.
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
        stack.children.push(LayoutChild {
            node: LayoutNode::Pane(new_pane),
            collapsed: false,
        });
        stack.weights.push(SizeWeight::default());
        stack.active = stack.children.len() - 1;
        for (index, child) in stack.children.iter_mut().enumerate() {
            child.collapsed = index != stack.active;
        }
    } else {
        let path = result.path_to(anchor).expect("presence checked above");
        let slot = result.node_at_mut(&path);
        *slot = LayoutNode::Split(SplitNode::stack(vec![anchor, new_pane], 1));
    }
    Ok(result)
}

/// A rejected in-place replacement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ReplaceError {
    /// The pane to replace is not in this layout.
    #[error("pane {target} is not in this layout")]
    PaneNotFound { target: PaneId },
}

impl DomainError for ReplaceError {
    fn category(&self) -> DomainCategory {
        DomainCategory::Layout
    }

    fn severity(&self) -> Severity {
        Severity::Recoverable
    }
}

/// Swap the pane shown at `target`'s position for `new_pane`, changing no
/// geometry at all.
///
/// The slot keeps its weights, its stack membership, its collapsed state,
/// and its active status — only the pane id changes, so a subsequent solve
/// places every other pane exactly where it was. This is the in-place
/// content swap; the prior pane id comes back so the caller can clean up
/// the runtime it replaced.
///
/// # Errors
///
/// [`ReplaceError::PaneNotFound`] when `target` has no leaf in `tree`; the
/// caller's tree is unchanged.
pub fn replace_leaf(
    tree: &LayoutNode,
    target: PaneId,
    new_pane: PaneId,
) -> Result<(LayoutNode, PaneId), ReplaceError> {
    let Some(path) = tree.path_to(target) else {
        return Err(ReplaceError::PaneNotFound { target });
    };
    let mut result = tree.clone();
    *result.node_at_mut(&path) = LayoutNode::Pane(new_pane);
    Ok((result, target))
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
    /// area first (ties keep layout order), followed in layout order by
    /// panes that cover none of it but still changed size — removing a
    /// stack member regrows the active member in place, without its rect
    /// ever touching the freed strip. The first entry is the natural
    /// focus-repair candidate — it visually took over the closed pane's
    /// space — and together the entries cover every pane whose PTY needs a
    /// resize. Collapsed stack members are never listed: their one-row
    /// header strip is Tile-owned chrome, not pane content, so crossing the
    /// old rect with it neither absorbs space nor makes a focus target.
    pub absorbed_by: Vec<PaneId>,
}

/// Remove the leaf holding `pane`; its space flows to the siblings on the
/// next solve.
///
/// Splits emptied by the removal are pruned so no region of the tab goes
/// dead, but a split left with a single child is kept — normalization is a
/// separate, explicit step. Inside a stack, removal keeps exactly one child
/// expanded: removing the active member activates the one that slides into
/// its place, removing any other member leaves the active one alone.
///
/// This is also the close-inside-a-stack path. A stack left with one member
/// becomes a plain leaf at the caller's next normalize; a stack left with
/// none is pruned here like any emptied split. A pane held open after its
/// process exits is *not* removed at all — it stays a live (dead-state)
/// member with a selectable header until something closes it explicitly.
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
        .filter(|&&(id, _)| !after.stack_headers.iter().any(|header| header.pane == id))
        .filter_map(|&(id, rect)| {
            if rect.is_empty() {
                return None;
            }
            let overlap = rect.intersection(old_rect).map_or(0, cell_area);
            let resized = before
                .panes
                .iter()
                .any(|&(before_id, before_rect)| before_id == id && before_rect.size != rect.size);
            (overlap > 0 || resized).then_some((id, overlap))
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
    split.active = split.active.min(split.children.len().saturating_sub(1));
    if split.direction == SplitDirection::Stacked {
        for (index, child) in split.children.iter_mut().enumerate() {
            child.collapsed = index != split.active;
        }
    }
}

#[cfg(test)]
mod tests;
