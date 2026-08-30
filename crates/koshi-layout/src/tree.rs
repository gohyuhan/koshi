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
pub(crate) fn split_axis(direction: Direction) -> SplitDirection {
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
    pub fn leaf_panes(&self) -> Vec<PaneId> {
        let mut panes = Vec::new();
        self.collect_leaf_panes(&mut panes);
        panes
    }

    /// Recursively appends leaf pane IDs to `out`, visiting depth-first in layout order.
    fn collect_leaf_panes(&self, out: &mut Vec<PaneId>) {
        match self {
            Self::Pane(id) => out.push(*id),
            Self::Split(split) => {
                for child in &split.children {
                    child.node.collect_leaf_panes(out);
                }
            }
        }
    }

    /// The first leaf pane in layout order, or `None` when this subtree
    /// holds no pane at all.
    pub(crate) fn first_leaf(&self) -> Option<PaneId> {
        match self {
            Self::Pane(id) => Some(*id),
            Self::Split(split) => split
                .children
                .iter()
                .find_map(|child| child.node.first_leaf()),
        }
    }

    /// `true` when some leaf of this subtree references `pane`.
    #[must_use]
    pub fn contains_pane(&self, pane: PaneId) -> bool {
        match self {
            Self::Pane(id) => *id == pane,
            Self::Split(split) => split
                .children
                .iter()
                .any(|child| child.node.contains_pane(pane)),
        }
    }

    /// The innermost (deepest-nested) stack whose subtree holds `pane`, or
    /// `None` when the pane is not inside any stack.
    pub fn stack_containing_mut(&mut self, pane: PaneId) -> Option<&mut SplitNode> {
        let path = self.path_to(pane)?;
        let deepest = (0..path.len()).rev().find(|&depth| {
            matches!(
                self.node_at(&path[..depth]),
                LayoutNode::Split(split) if split.direction == SplitDirection::Stacked
            )
        })?;
        Some(self.split_at_mut(&path[..deepest]))
    }

    /// The child index taken at each split from this node down to the leaf
    /// holding `pane`, or `None` when the pane is not in this subtree. A
    /// bare pane yields an empty path.
    ///
    /// A path is valid only against the exact tree it was computed from.
    pub(crate) fn path_to(&self, pane: PaneId) -> Option<Vec<usize>> {
        fn descend(node: &LayoutNode, pane: PaneId, path: &mut Vec<usize>) -> bool {
            match node {
                LayoutNode::Pane(id) => *id == pane,
                LayoutNode::Split(split) => {
                    for (index, child) in split.children.iter().enumerate() {
                        path.push(index);
                        if descend(&child.node, pane, path) {
                            return true;
                        }
                        path.pop();
                    }
                    false
                }
            }
        }

        let mut path = Vec::new();
        descend(self, pane, &mut path).then_some(path)
    }

    /// The node reached by walking `path` child indices from this node.
    /// `path` must come from [`LayoutNode::path_to`] on this same tree.
    /// Panics when `path` steps into a pane or past a split's last child.
    pub(crate) fn node_at(&self, path: &[usize]) -> &LayoutNode {
        let mut node = self;
        for &index in path {
            let LayoutNode::Split(split) = node else {
                unreachable!("path was built over this tree");
            };
            node = &split.children[index].node;
        }
        node
    }

    /// Mutable variant of [`LayoutNode::node_at`].
    pub(crate) fn node_at_mut(&mut self, path: &[usize]) -> &mut LayoutNode {
        let mut node = self;
        for &index in path {
            let LayoutNode::Split(split) = node else {
                unreachable!("path was built over this tree");
            };
            node = &mut split.children[index].node;
        }
        node
    }

    /// Like [`LayoutNode::node_at`], for paths known to end at a split.
    /// Panics when the node at `path` is a pane.
    pub(crate) fn split_at(&self, path: &[usize]) -> &SplitNode {
        match self.node_at(path) {
            LayoutNode::Split(split) => split,
            LayoutNode::Pane(_) => unreachable!("path was built over this tree"),
        }
    }

    /// Mutable variant of [`LayoutNode::split_at`].
    pub(crate) fn split_at_mut(&mut self, path: &[usize]) -> &mut SplitNode {
        match self.node_at_mut(path) {
            LayoutNode::Split(split) => split,
            LayoutNode::Pane(_) => unreachable!("path was built over this tree"),
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
    /// The child slots, in layout order (left-to-right or top-to-bottom).
    pub children: Vec<LayoutChild>,
    /// Per-child size constraints, parallel to `children`.
    pub weights: Vec<SizeWeight>,
    /// Index of the active child. Only meaningful for `Stacked` splits,
    /// where it names the one expanded member (all others are collapsed).
    /// Directional splits carry `0`.
    pub active: usize,
}

impl SplitNode {
    /// A split of `direction` over `children` sharing space evenly: one
    /// default weight per child, `active` 0. `children` keep their
    /// `collapsed` flags as given.
    #[must_use]
    pub fn with_equal_weights(direction: SplitDirection, children: Vec<LayoutChild>) -> Self {
        let weights = vec![SizeWeight::default(); children.len()];
        Self {
            direction,
            children,
            weights,
            active: 0,
        }
    }

    /// A stack of `panes`: the child at `active` is expanded, the rest are
    /// collapsed to headers. `active` is clamped to the last child index.
    /// One pane yields a one-member stack. No panes yield an empty stack
    /// with `active` 0.
    #[must_use]
    pub fn stack(panes: Vec<PaneId>, active: usize) -> Self {
        let active = active.min(panes.len().saturating_sub(1));
        let children = panes
            .iter()
            .enumerate()
            .map(|(index, &pane)| LayoutChild {
                node: LayoutNode::Pane(pane),
                collapsed: index != active,
            })
            .collect();
        let weights = vec![SizeWeight::default(); panes.len()];
        Self {
            direction: SplitDirection::Stacked,
            children,
            weights,
            active,
        }
    }

    /// `active` clamped to the last child index; `0` for an empty split.
    pub(crate) fn active_index(&self) -> usize {
        self.active.min(self.children.len().saturating_sub(1))
    }
}

/// One child slot of a split.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayoutChild {
    /// The subtree occupying this slot.
    pub node: LayoutNode,
    /// `true` when a stacked child is collapsed to its one-row header.
    /// Directional splits never collapse children; this stays `false` there.
    pub collapsed: bool,
}

impl LayoutChild {
    /// An expanded child (`collapsed: false`), the only state directional
    /// splits use.
    #[must_use]
    pub fn new(node: LayoutNode) -> Self {
        Self {
            node,
            collapsed: false,
        }
    }
}

#[cfg(test)]
mod tests;
