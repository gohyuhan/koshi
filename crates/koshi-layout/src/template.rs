//! Layout templates: a pane arrangement described before any pane exists.
//!
//! A profile file describes tabs, splits, and the panes to spawn in them. A
//! template is a [`crate::tree::LayoutNode`] tree with the pane ids
//! abstracted away: interior nodes mirror [`SplitNode`] field for field
//! (direction, ordered children, parallel weights, active member), and each
//! leaf carries *what to put there* — a terminal command or a plugin name —
//! instead of *which pane is there*.
//!
//! To instantiate a template, create one pane per leaf, then call
//! [`TemplateNode::to_layout_node`] with the new ids in layout order.
//! Example: a template `horizontal(pane "nvim", pane)` plus ids `[7, 8]`
//! yields `Split(Horizontal, [Pane(7), Pane(8)])`, the same tree a runtime
//! split of pane 7 produces.

use std::collections::BTreeMap;
use std::path::PathBuf;

use koshi_core::error::{DomainCategory, DomainError, Severity};
use koshi_core::geometry::SplitDirection;
use koshi_core::ids::PaneId;
use thiserror::Error;

use crate::size::SizeWeight;
use crate::tree::{LayoutNode, SplitNode};

#[cfg(test)]
mod tests;

/// A whole profile file: the tabs it defines, which one starts focused, and
/// whether the first client to attach starts in locked input mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileTemplate {
    /// The tabs in file order. Never empty: a profile without tabs is a
    /// parse error.
    pub tabs: Vec<TabTemplate>,
    /// Index into `tabs` of the tab selected when the profile opens.
    pub focused_tab: usize,
    /// True when the file carries the `lock` marker. The session this
    /// template seeds starts its first client in
    /// [`LockMode::Locked`](koshi_core::lock::LockMode::Locked), and no
    /// client after that one.
    pub locked: bool,
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
    /// All leaves in layout order: depth-first, children in order. This is
    /// the order [`LayoutNode::leaf_panes`] yields for the instantiated
    /// tree: leaf `i` here is the slot `ids[i]` fills in
    /// [`TemplateNode::to_layout_node`].
    #[must_use]
    pub fn leaves(&self) -> Vec<&LeafTemplate> {
        let mut leaves = Vec::new();
        self.collect_leaves(&mut leaves);
        leaves
    }

    /// How many leaves this subtree holds — the length
    /// [`TemplateNode::leaves`] returns, counted without building the list.
    fn leaf_count(&self) -> usize {
        match self {
            Self::Leaf(_) => 1,
            Self::Split(split) => split.children.iter().map(|child| child.leaf_count()).sum(),
        }
    }

    /// Recursively appends leaves to `out`, depth-first in layout order.
    fn collect_leaves<'a>(&'a self, out: &mut Vec<&'a LeafTemplate>) {
        match self {
            Self::Leaf(leaf) => out.push(leaf),
            Self::Split(split) => {
                for child in &split.children {
                    child.collect_leaves(out);
                }
            }
        }
    }

    /// Index (in [`TemplateNode::leaves`] order) of the first leaf a user
    /// sees. At a stacked node the walk descends into the child at `active`,
    /// skipping the leaves of the collapsed members before it; at a
    /// directional split it descends into the first child. Example:
    /// `horizontal(stack(a, b expanded), c)` yields `1` — leaf `b`, not the
    /// collapsed `a`.
    ///
    /// A split with no children contributes `0`: the result is the number of
    /// leaves before that subtree. A stacked split whose `active` is past its
    /// last child expands its last child, the same clamp the solver applies
    /// once the template is instantiated.
    #[must_use]
    pub fn first_visible_leaf(&self) -> usize {
        match self {
            Self::Leaf(_) => 0,
            Self::Split(split) => {
                let pick = match split.direction {
                    SplitDirection::Stacked => {
                        split.active.min(split.children.len().saturating_sub(1))
                    }
                    SplitDirection::Horizontal | SplitDirection::Vertical => 0,
                };
                let Some(child) = split.children.get(pick) else {
                    return 0;
                };
                let skipped: usize = split.children[..pick]
                    .iter()
                    .map(|earlier| earlier.leaf_count())
                    .sum();
                skipped + child.first_visible_leaf()
            }
        }
    }

    /// Builds the live tree this template describes. `ids` supplies one
    /// [`PaneId`] per leaf, in layout order: `ids[i]` fills the `i`-th leaf
    /// of [`TemplateNode::leaves`]. Structure, directions, weights, and
    /// active members carry over unchanged.
    ///
    /// # Errors
    /// [`TemplateError::PaneCountMismatch`] when `ids` does not hold exactly
    /// one id per leaf.
    pub fn to_layout_node(&self, ids: &[PaneId]) -> Result<LayoutNode, TemplateError> {
        let expected = self.leaf_count();
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
                    .map(|child| child.build(ids, next))
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
    /// Working directory as written in the file: no `~` expansion and no
    /// path resolution.
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
    /// The child subtrees, in layout order.
    pub children: Vec<TemplateNode>,
    /// Per-child size constraints, parallel to `children`.
    pub weights: Vec<SizeWeight>,
    /// Index of the active child. Only meaningful for `Stacked` nodes,
    /// where it names the one expanded member.
    pub active: usize,
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

impl DomainError for TemplateError {
    fn category(&self) -> DomainCategory {
        DomainCategory::Layout
    }

    fn severity(&self) -> Severity {
        Severity::Recoverable
    }
}
