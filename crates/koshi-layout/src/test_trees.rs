//! Tree builders shared by the unit tests of more than one module.

use koshi_core::geometry::SplitDirection;
use koshi_core::ids::PaneId;

use crate::tree::{LayoutChild, LayoutNode, SplitNode};

/// A left-leaning tree of `panes.len() - 1` splits alternating
/// horizontal/vertical by depth. Index 0 is the outermost leaf, the last index
/// the deepest.
///
/// Three panes `[a, b, c]` build a horizontal split of `a` against a vertical
/// split of `b` against `c`.
pub(crate) fn deep_alternating(panes: &[PaneId]) -> LayoutNode {
    let (&last, rest) = panes
        .split_last()
        .expect("deep_alternating needs at least one pane");
    let mut node = LayoutNode::Pane(last);
    for (index, &pane) in rest.iter().enumerate().rev() {
        let direction = if index % 2 == 0 {
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
