//! Layout templates: a pane arrangement described before any pane exists.
//!
//! A layout file describes tabs, splits, and the panes to spawn in them —
//! but a [`crate::tree::LayoutNode`] leaf is a live [`PaneId`], and no panes
//! exist while a file is being read. A template is the same tree with the
//! ids abstracted away: interior nodes mirror [`SplitNode`] field for field
//! (direction, ordered children, parallel weights, active member), and each
//! leaf carries *what to put there* — a terminal command or a plugin name —
//! instead of *which pane is there*.
//!
//! Instantiation closes the gap: create one pane per leaf, then call
//! [`TemplateNode::to_layout_node`] with the new ids in layout order to get
//! the live tree. Example: a template `horizontal(pane "nvim", pane)` plus
//! ids `[7, 8]` yields `Split(Horizontal, [Pane(7), Pane(8)])` — the same
//! tree a runtime split of pane 7 would have produced, so file-defined and
//! runtime-built layouts stay one model.

use std::collections::BTreeMap;
use std::path::PathBuf;

use koshi_core::geometry::SplitDirection;
use koshi_core::ids::PaneId;
use thiserror::Error;

use crate::size::SizeWeight;
use crate::tree::{LayoutChild, LayoutNode, SplitNode};

#[cfg(test)]
mod tests;

/// A whole layout file: the tabs it defines and which one starts focused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayoutTemplate {
    /// The tabs in file order. Never empty: a layout without tabs is a
    /// parse error, not a representable template.
    pub tabs: Vec<TabTemplate>,
    /// Index into `tabs` of the tab selected when the layout opens.
    pub focused_tab: usize,
}

/// One tab's pane arrangement and its initial focus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabTemplate {
    /// The tab's layout tree.
    pub root: TemplateNode,
    /// Index into the root's leaves (layout order) of the pane focused when
    /// this tab is first shown.
    pub focused_leaf: usize,
}

/// A node in a template tree: a leaf to fill with a pane, or a split.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TemplateNode {
    /// A slot one pane will occupy.
    Leaf(LeafTemplate),
    /// An interior node dividing (or stacking) its rectangle, mirroring
    /// [`SplitNode`].
    Split(TemplateSplit),
}

impl TemplateNode {
    /// All leaves in layout order: depth-first, children in order — the
    /// same order [`LayoutNode::leaf_panes`] yields for the instantiated
    /// tree, so index `i` here matches pane id `i` there.
    #[must_use]
    pub fn leaves(&self) -> Vec<&LeafTemplate> {
        let mut leaves = Vec::new();
        self.collect_leaves(&mut leaves);
        leaves
    }

    /// Recursively appends leaves to `out`, depth-first in layout order.
    fn collect_leaves<'a>(&'a self, out: &mut Vec<&'a LeafTemplate>) {
        match self {
            Self::Leaf(leaf) => out.push(leaf),
            Self::Split(split) => {
                for child in &split.children {
                    child.node.collect_leaves(out);
                }
            }
        }
    }

    /// Index (in [`TemplateNode::leaves`] order) of the first leaf a user
    /// actually sees: at a stacked node only the active member is visible,
    /// so collapsed members are skipped; at a directional split the first
    /// child wins. Example: `horizontal(stack(a, b expanded), c)` yields
    /// `1` — leaf `b`, not the collapsed `a`. This is the leaf initial
    /// focus falls on when a layout names none.
    ///
    /// A split with no children (representable only in degraded trees that
    /// never instantiate) yields `0`.
    #[must_use]
    pub fn first_visible_leaf(&self) -> usize {
        match self {
            Self::Leaf(_) => 0,
            Self::Split(split) => {
                let pick = match split.direction {
                    SplitDirection::Stacked => split.active,
                    SplitDirection::Horizontal | SplitDirection::Vertical => 0,
                };
                let Some(child) = split.children.get(pick) else {
                    return 0;
                };
                let skipped: usize = split.children[..pick]
                    .iter()
                    .map(|earlier| earlier.node.leaves().len())
                    .sum();
                skipped + child.node.first_visible_leaf()
            }
        }
    }

    /// Builds the live tree this template describes. `ids` supplies one
    /// [`PaneId`] per leaf, in layout order: `ids[i]` fills the `i`-th leaf
    /// of [`TemplateNode::leaves`]. Structure, directions, weights, active
    /// members, and collapsed flags carry over unchanged.
    ///
    /// # Errors
    /// [`TemplateError::PaneCountMismatch`] when `ids` does not hold exactly
    /// one id per leaf.
    pub fn to_layout_node(&self, ids: &[PaneId]) -> Result<LayoutNode, TemplateError> {
        let expected = self.leaves().len();
        if ids.len() != expected {
            return Err(TemplateError::PaneCountMismatch {
                expected,
                got: ids.len(),
            });
        }
        let mut next = 0;
        Ok(self.build(ids, &mut next))
    }

    /// Recursively builds the live subtree, consuming `ids[*next]` at each
    /// leaf in layout order.
    fn build(&self, ids: &[PaneId], next: &mut usize) -> LayoutNode {
        match self {
            Self::Leaf(_) => {
                let id = ids[*next];
                *next += 1;
                LayoutNode::Pane(id)
            }
            Self::Split(split) => {
                let children = split
                    .children
                    .iter()
                    .map(|child| LayoutChild {
                        node: child.node.build(ids, next),
                        collapsed: child.collapsed,
                    })
                    .collect();
                LayoutNode::Split(SplitNode {
                    direction: split.direction,
                    children,
                    weights: split.weights.clone(),
                    active: split.active,
                })
            }
        }
    }
}

/// What fills a leaf slot: a terminal pane or a plugin pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeafTemplate {
    /// A terminal pane running a command (or the default shell).
    Terminal(TerminalTemplate),
    /// A plugin pane rendered by the named plugin.
    Plugin(PluginTemplate),
}

/// A terminal pane to spawn: what to run, where, and with which extra
/// environment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TerminalTemplate {
    /// The command to run. `None` runs the user's default shell.
    pub command: Option<CommandTemplate>,
    /// Working directory, verbatim from the file; expansion (`~`) and
    /// resolution happen at spawn time, not here.
    pub cwd: Option<PathBuf>,
    /// Extra environment variables set for the spawned process.
    pub env: BTreeMap<String, String>,
}

/// A program invocation: the executable and its arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandTemplate {
    /// The program to execute.
    pub program: PathBuf,
    /// Arguments passed to the program, in order.
    pub args: Vec<String>,
}

/// A plugin pane to open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginTemplate {
    /// The plugin's identifier, e.g. `"session-manager"`.
    pub name: String,
}

/// An interior template node, mirroring [`SplitNode`]: `children` and
/// `weights` are parallel, and `active` names the expanded member of a
/// [`SplitDirection::Stacked`] node (directional nodes carry it as zero).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateSplit {
    /// How the children divide this node's rectangle.
    pub direction: SplitDirection,
    /// The child slots, in layout order.
    pub children: Vec<TemplateChild>,
    /// Per-child size constraints, parallel to `children`.
    pub weights: Vec<SizeWeight>,
    /// Index of the active child. Only meaningful for `Stacked` nodes,
    /// where it names the one expanded member.
    pub active: usize,
}

/// One child slot of a template split, mirroring [`LayoutChild`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateChild {
    /// The subtree occupying this slot.
    pub node: TemplateNode,
    /// `true` when a stacked child starts collapsed to its one-row header.
    /// Directional splits never collapse children; this stays `false` there.
    pub collapsed: bool,
}

/// A failed template instantiation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum TemplateError {
    /// The id list does not pair one id with each leaf.
    #[error("template has {expected} pane slots but {got} pane ids were supplied")]
    PaneCountMismatch {
        /// Leaf count of the template.
        expected: usize,
        /// Length of the supplied id slice.
        got: usize,
    },
}
