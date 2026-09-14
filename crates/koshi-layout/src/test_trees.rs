//! Tree builders shared by the unit tests of more than one module.

use koshi_core::geometry::SplitDirection;
use koshi_core::ids::PaneId;

use crate::tree::{LayoutNode, SplitNode};

/// A chain of `panes.len() - 1` two-child splits. Each split holds one leaf
/// as its first child and the rest of the chain as its second child. The
/// split at depth 0 is horizontal, at depth 1 vertical, and so on in turn.
/// Index 0 is the outermost leaf, the last index the deepest.
///
/// Three panes `[a, b, c]` build a horizontal split of `a` against a vertical
/// split of `b` against `c`. One pane builds a bare leaf.
///
/// Panics when `pane_ids` is empty.
pub(crate) fn build_deep_alternating_layout(pane_ids: &[PaneId]) -> LayoutNode {
    let (&last_pane_id, preceding_pane_ids) = pane_ids
        .split_last()
        .expect("build_deep_alternating_layout needs at least one pane");
    let mut layout_tree = LayoutNode::Pane(last_pane_id);
    for (split_depth, &pane_id) in preceding_pane_ids.iter().enumerate().rev() {
        let split_direction = if split_depth % 2 == 0 {
            SplitDirection::Horizontal
        } else {
            SplitDirection::Vertical
        };
        layout_tree = LayoutNode::Split(SplitNode::with_equal_weights(
            split_direction,
            vec![LayoutNode::Pane(pane_id), layout_tree],
        ));
    }
    layout_tree
}
