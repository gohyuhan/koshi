//! Tree builders shared by the unit tests of more than one module.

use koshi_core::geometry::SplitDirection;
use koshi_core::ids::PaneId;

use crate::tree::{LayoutChild, LayoutNode, SplitNode};

/// A chain of `panes.len() - 1` two-child splits. Each split holds one leaf
/// as its first child and the rest of the chain as its second child. The
/// split at depth 0 is horizontal, at depth 1 vertical, and so on in turn.
/// Index 0 is the outermost leaf, the last index the deepest.
///
/// Three panes `[a, b, c]` build a horizontal split of `a` against a vertical
/// split of `b` against `c`. One pane builds a bare leaf.
///
/// Panics when `panes` is empty.
pub(crate) fn deep_alternating(panes: &[PaneId]) -> LayoutNode {
    let (&last, rest) = panes
        .split_last()
        .expect("deep_alternating needs at least one pane");
    let mut node = LayoutNode::Pane(last);
    for (depth, &pane) in rest.iter().enumerate().rev() {
        let direction = if depth % 2 == 0 {
            SplitDirection::Horizontal
        } else {
            SplitDirection::Vertical
        };
        node = LayoutNode::Split(SplitNode::with_equal_weights(
            direction,
            vec![
                LayoutChild::new(LayoutNode::Pane(pane)),
                LayoutChild::new(node),
            ],
        ));
    }
    node
}
