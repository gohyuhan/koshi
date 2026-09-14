//! The layout tree: which panes a tab shows and how they divide its area.
//!
//! A tab's pane arrangement is a tree. Leaves are panes; interior nodes are
//! splits that divide their rectangle among children along one axis. A
//! `Stacked` split shares its rectangle instead: exactly one child is
//! expanded and the rest collapse to one-row headers.
//!
//! The tree stores structure and relative sizes, never solved geometry. The
//! solver maps a tree plus a tab rectangle to pane rectangles on every solve.
//!
//! Nodes are plain serializable data. Structural edits (split, remove,
//! normalize) live in sibling modules and return new trees. This module holds
//! the node types, the read-only walks, and the accessors that hand out one
//! node for an in-place edit.

use koshi_core::geometry::{Direction, SplitDirection};
use koshi_core::ids::PaneId;
use serde::{Deserialize, Serialize};

use crate::size::SizeWeight;

/// The split axis a cardinal direction runs on: [`SplitDirection::Horizontal`]
/// for `Left` and `Right`, [`SplitDirection::Vertical`] for `Up` and `Down`.
pub(crate) fn compute_split_direction(direction: Direction) -> SplitDirection {
    match direction {
        Direction::Left | Direction::Right => SplitDirection::Horizontal,
        Direction::Up | Direction::Down => SplitDirection::Vertical,
    }
}

/// A node in the layout tree: a single pane, or a split holding children.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LayoutNode {
    /// A leaf. The pane fills this node's entire rectangle.
    Pane(PaneId),
    /// An interior node dividing (or stacking) its rectangle among children.
    Split(SplitNode),
}

impl LayoutNode {
    /// All leaf pane ids in layout order: depth-first, children in order.
    /// The same tree yields the same list on every call. The solver lists
    /// solved and suppressed panes in this order.
    #[must_use]
    pub fn list_leaf_pane_ids(&self) -> Vec<PaneId> {
        let mut leaf_pane_ids = Vec::new();
        self.collect_leaf_pane_ids(&mut leaf_pane_ids);
        leaf_pane_ids
    }

    /// Recursively appends leaf pane IDs to `leaf_pane_ids`, visiting depth-first in layout order.
    fn collect_leaf_pane_ids(&self, leaf_pane_ids: &mut Vec<PaneId>) {
        match self {
            Self::Pane(pane_id) => leaf_pane_ids.push(*pane_id),
            Self::Split(split) => {
                for child_node in &split.children {
                    child_node.collect_leaf_pane_ids(leaf_pane_ids);
                }
            }
        }
    }

    /// The first leaf pane in layout order, or `None` when this subtree
    /// holds no pane at all.
    pub(crate) fn find_first_leaf_pane_id(&self) -> Option<PaneId> {
        match self {
            Self::Pane(pane_id) => Some(*pane_id),
            Self::Split(split) => split
                .children
                .iter()
                .find_map(|child_node| child_node.find_first_leaf_pane_id()),
        }
    }

    /// `true` when some leaf of this subtree references `pane_id`.
    #[must_use]
    pub fn contains_pane(&self, pane_id: PaneId) -> bool {
        match self {
            Self::Pane(candidate_pane_id) => *candidate_pane_id == pane_id,
            Self::Split(split) => split
                .children
                .iter()
                .any(|child_node| child_node.contains_pane(pane_id)),
        }
    }

    /// The innermost (deepest-nested) stack whose subtree holds `pane_id`, or
    /// `None` when the pane is not inside any stack.
    pub fn find_containing_stack_mut(&mut self, pane_id: PaneId) -> Option<&mut SplitNode> {
        let pane_path = self.find_pane_path(pane_id)?;
        let deepest_stack_depth = (0..pane_path.len()).rev().find(|&stack_depth| {
            matches!(
                self.get_node_at_path(&pane_path[..stack_depth]),
                LayoutNode::Split(split) if split.direction == SplitDirection::Stacked
            )
        })?;
        Some(self.get_split_at_path_mut(&pane_path[..deepest_stack_depth]))
    }

    /// The child index taken at each split from this node down to the leaf
    /// holding `pane_id`, or `None` when the pane is not in this subtree. A
    /// bare pane yields an empty path.
    ///
    /// A path is valid only against the exact tree it was computed from.
    pub(crate) fn find_pane_path(&self, pane_id: PaneId) -> Option<Vec<usize>> {
        fn find_pane_in_subtree(
            layout_node: &LayoutNode,
            pane_id: PaneId,
            pane_path: &mut Vec<usize>,
        ) -> bool {
            match layout_node {
                LayoutNode::Pane(layout_pane_id) => *layout_pane_id == pane_id,
                LayoutNode::Split(split) => {
                    for (child_index, child_node) in split.children.iter().enumerate() {
                        pane_path.push(child_index);
                        if find_pane_in_subtree(child_node, pane_id, pane_path) {
                            return true;
                        }
                        pane_path.pop();
                    }
                    false
                }
            }
        }

        let mut pane_path = Vec::new();
        find_pane_in_subtree(self, pane_id, &mut pane_path).then_some(pane_path)
    }

    /// The node reached by walking `layout_path` child indices from this node.
    /// `layout_path` must come from [`LayoutNode::find_pane_path`] on this same tree.
    /// Panics when `layout_path` steps into a pane or past a split's last child.
    pub(crate) fn get_node_at_path(&self, layout_path: &[usize]) -> &LayoutNode {
        let mut layout_node = self;
        for &child_index in layout_path {
            let LayoutNode::Split(split) = layout_node else {
                unreachable!("layout path was built over this tree");
            };
            layout_node = &split.children[child_index];
        }
        layout_node
    }

    /// Mutable variant of [`LayoutNode::get_node_at_path`].
    pub(crate) fn get_node_at_path_mut(&mut self, layout_path: &[usize]) -> &mut LayoutNode {
        let mut layout_node = self;
        for &child_index in layout_path {
            let LayoutNode::Split(split) = layout_node else {
                unreachable!("layout path was built over this tree");
            };
            layout_node = &mut split.children[child_index];
        }
        layout_node
    }

    /// Like [`LayoutNode::get_node_at_path`], for paths known to end at a split.
    /// Panics when the node at `layout_path` is a pane.
    pub(crate) fn get_split_at_path(&self, layout_path: &[usize]) -> &SplitNode {
        match self.get_node_at_path(layout_path) {
            LayoutNode::Split(split) => split,
            LayoutNode::Pane(_) => unreachable!("layout path was built over this tree"),
        }
    }

    /// Mutable variant of [`LayoutNode::get_split_at_path`].
    pub(crate) fn get_split_at_path_mut(&mut self, layout_path: &[usize]) -> &mut SplitNode {
        match self.get_node_at_path_mut(layout_path) {
            LayoutNode::Split(split) => split,
            LayoutNode::Pane(_) => unreachable!("layout path was built over this tree"),
        }
    }
}

/// An interior node: children share this node's rectangle.
///
/// `children` and `weights` are parallel: `weights[i]` sizes `children[i]`
/// along the split axis. Edits must always grow or shrink them together.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SplitNode {
    /// How the children divide this node's rectangle.
    pub direction: SplitDirection,
    /// The child subtrees, in layout order (left-to-right or top-to-bottom).
    #[serde(deserialize_with = "children_from_wire")]
    pub children: Vec<LayoutNode>,
    /// Per-child size constraints, parallel to `children`.
    pub weights: Vec<SizeWeight>,
    /// Index of the expanded child. Only meaningful for `Stacked` splits,
    /// where it names the one expanded member; every other member is
    /// collapsed to its one-row header. Directional splits carry `0` and
    /// collapse no child.
    #[serde(rename = "active")]
    pub active_child_index: usize,
}

impl SplitNode {
    /// A split of `direction` over `children` sharing space evenly: one
    /// default weight per child, `active_child_index` 0.
    #[must_use]
    pub fn with_equal_weights(direction: SplitDirection, children: Vec<LayoutNode>) -> Self {
        let weights = vec![SizeWeight::default(); children.len()];
        Self {
            direction,
            children,
            weights,
            active_child_index: 0,
        }
    }

    /// A stack of `pane_ids`: the child at `active_child_index` is expanded,
    /// the rest are collapsed to headers. `active_child_index` is clamped to
    /// the last child index. One pane yields a one-member stack. No panes yield
    /// an empty stack with `active_child_index` 0.
    #[must_use]
    pub fn from_stacked_pane_ids(pane_ids: Vec<PaneId>, active_child_index: usize) -> Self {
        let active_child_index = active_child_index.min(pane_ids.len().saturating_sub(1));
        let weights = vec![SizeWeight::default(); pane_ids.len()];
        let children = pane_ids.into_iter().map(LayoutNode::Pane).collect();
        Self {
            direction: SplitDirection::Stacked,
            children,
            weights,
            active_child_index,
        }
    }

    /// `active_child_index` clamped to the last child index; `0` for an empty split.
    #[must_use]
    pub fn get_active_child_index(&self) -> usize {
        self.active_child_index
            .min(self.children.len().saturating_sub(1))
    }

    /// `true` when the child at `child_index` is collapsed to its one-row header:
    /// this split is `Stacked` and `child_index` is not
    /// [`SplitNode::get_active_child_index`]. Always `false` for a directional
    /// split, and `false` for a `child_index` past the last child.
    #[must_use]
    pub fn is_child_collapsed(&self, child_index: usize) -> bool {
        self.direction == SplitDirection::Stacked
            && child_index < self.children.len()
            && child_index != self.get_active_child_index()
    }
}

/// One entry of [`SplitNode::children`] as it arrives: the node itself, or
/// the `{"node": …}` record a koshi before this one wrote.
#[derive(Deserialize)]
#[serde(untagged)]
enum ChildOnWire {
    /// The node written directly.
    Bare(LayoutNode),
    /// The node inside a one-field record.
    Wrapped {
        /// The subtree the record holds.
        node: LayoutNode,
    },
}

/// Read [`SplitNode::children`] from either shape: a list of nodes, or a list
/// of `{"node": …}` records. Both yield the same nodes, in the same order.
/// Writing always uses the first shape.
///
/// Example — `[{"Pane":1}]` and `[{"node":{"Pane":1}}]` both read back as one
/// [`LayoutNode::Pane`] holding pane `1`.
///
/// # Errors
/// Returns whatever `deserializer` reports for an entry matching neither
/// shape.
fn children_from_wire<'de, D>(deserializer: D) -> Result<Vec<LayoutNode>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let child_records = Vec::<ChildOnWire>::deserialize(deserializer)?;
    Ok(child_records
        .into_iter()
        .map(|child_record| match child_record {
            ChildOnWire::Bare(layout_node) | ChildOnWire::Wrapped { node: layout_node } => {
                layout_node
            }
        })
        .collect())
}

#[cfg(test)]
mod tests;
