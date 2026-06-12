//! Structural edits to the layout tree: splitting panes in, taking them out,
//! swapping their contents.
//!
//! Every edit is pure: it borrows the current tree and returns a new one,
//! leaving the input untouched. A failed edit returns an error and changes
//! nothing, so callers can validate-and-apply in one step — there is no
//! half-edited tree to roll back.

use thiserror::Error;
use tile_core::error::{DomainCategory, DomainError, Severity};
use tile_core::geometry::{Direction, SplitDirection};
use tile_core::ids::PaneId;

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
