//! Layout templates: a pane arrangement described before any pane exists.
//!
//! A profile file describes tabs, splits, and the panes to spawn in them. A
//! template is a [`crate::tree::LayoutNode`] tree with the pane ids
//! abstracted away: interior nodes mirror [`SplitNode`] field for field
//! (direction, ordered children, parallel weights, expanded member), and each
//! leaf carries *what to put there* — a terminal command or a plugin name —
//! instead of *which pane is there*.
//!
//! To instantiate a template, create one pane per leaf, then call
//! [`TemplateNode::build_layout_node`] with the new ids in layout order.
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
    pub focused_tab_index: usize,
    /// True when the file carries the `lock` marker. The session this
    /// template seeds starts its first client in
    /// [`LockMode::Locked`](koshi_core::lock::LockMode::Locked), and no
    /// client after that one.
    pub is_locked: bool,
}

/// One tab's pane arrangement and its initial focus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabTemplate {
    /// The tab's layout tree.
    pub root: TemplateNode,
    /// Index into the root's leaves (layout order) of the pane focused when
    /// this tab is first shown.
    pub focused_leaf_index: usize,
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
    /// the order [`LayoutNode::list_leaf_pane_ids`] yields for the instantiated
    /// tree: leaf `i` here is the slot `ids[i]` fills in
    /// [`TemplateNode::build_layout_node`].
    #[must_use]
    pub fn list_leaf_templates(&self) -> Vec<&LeafTemplate> {
        let mut leaf_templates = Vec::new();
        self.append_leaf_templates(&mut leaf_templates);
        leaf_templates
    }

    /// How many leaves this subtree holds — the length
    /// [`TemplateNode::list_leaf_templates`] returns, counted without building
    /// the list.
    fn count_leaf_templates(&self) -> usize {
        match self {
            Self::Leaf(_) => 1,
            Self::Split(split) => split
                .children
                .iter()
                .map(|child| child.count_leaf_templates())
                .sum(),
        }
    }

    /// Recursively appends leaves to `leaf_templates`, depth-first in layout
    /// order.
    fn append_leaf_templates<'a>(&'a self, leaf_templates: &mut Vec<&'a LeafTemplate>) {
        match self {
            Self::Leaf(leaf) => leaf_templates.push(leaf),
            Self::Split(split) => {
                for child in &split.children {
                    child.append_leaf_templates(leaf_templates);
                }
            }
        }
    }

    /// Index (in [`TemplateNode::list_leaf_templates`] order) of the first leaf a user
    /// sees. At a stacked node the walk descends into the child at
    /// `active_child_index`,
    /// skipping the leaves of the collapsed members before it; at a
    /// directional split it descends into the first child. Example:
    /// `horizontal(stack(a, b expanded), c)` yields `1` — leaf `b`, not the
    /// collapsed `a`.
    ///
    /// A split with no children contributes `0`: the result is the number of
    /// leaves before that subtree. A stacked split whose `active_child_index` is past its
    /// last child expands its last child, the same clamp the solver applies
    /// once the template is instantiated.
    #[must_use]
    pub fn find_first_visible_leaf_index(&self) -> usize {
        match self {
            Self::Leaf(_) => 0,
            Self::Split(split) => {
                let active_child_index = match split.direction {
                    SplitDirection::Stacked => split
                        .active_child_index
                        .min(split.children.len().saturating_sub(1)),
                    SplitDirection::Horizontal | SplitDirection::Vertical => 0,
                };
                let Some(child) = split.children.get(active_child_index) else {
                    return 0;
                };
                let skipped_leaf_count: usize = split.children[..active_child_index]
                    .iter()
                    .map(|earlier| earlier.count_leaf_templates())
                    .sum();
                skipped_leaf_count + child.find_first_visible_leaf_index()
            }
        }
    }

    /// Builds the live tree this template describes. `pane_ids` supplies one
    /// [`PaneId`] per leaf, in layout order: `pane_ids[i]` fills the `i`-th leaf
    /// of [`TemplateNode::list_leaf_templates`]. Structure, directions, weights, and
    /// expanded members carry over unchanged.
    ///
    /// # Errors
    /// [`TemplateError::PaneCountMismatch`] when `pane_ids` does not hold exactly
    /// one id per leaf.
    pub fn build_layout_node(&self, pane_ids: &[PaneId]) -> Result<LayoutNode, TemplateError> {
        let expected_leaf_count = self.count_leaf_templates();
        if pane_ids.len() != expected_leaf_count {
            return Err(TemplateError::PaneCountMismatch {
                expected_leaf_count,
                provided_pane_id_count: pane_ids.len(),
            });
        }
        let mut next_pane_id_index = 0;
        Ok(self.build_layout_subtree(pane_ids, &mut next_pane_id_index))
    }

    /// Recursively builds the live subtree, consuming `pane_ids[*next_pane_id_index]` at each
    /// leaf in layout order.
    fn build_layout_subtree(
        &self,
        pane_ids: &[PaneId],
        next_pane_id_index: &mut usize,
    ) -> LayoutNode {
        match self {
            Self::Leaf(_) => {
                let pane_id = pane_ids[*next_pane_id_index];
                *next_pane_id_index += 1;
                LayoutNode::Pane(pane_id)
            }
            Self::Split(split) => {
                let layout_children = split
                    .children
                    .iter()
                    .map(|child| child.build_layout_subtree(pane_ids, next_pane_id_index))
                    .collect();
                LayoutNode::Split(SplitNode {
                    direction: split.direction,
                    children: layout_children,
                    weights: split.weights.clone(),
                    active_child_index: split.active_child_index,
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
    pub working_directory: Option<PathBuf>,
    /// Extra environment variables set for the spawned process.
    pub environment_variables: BTreeMap<String, String>,
}

/// A program invocation: the executable and its arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandTemplate {
    /// The program to execute.
    pub program: PathBuf,
    /// Arguments passed to the program, in order.
    pub arguments: Vec<String>,
}

/// A plugin pane to open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginTemplate {
    /// The plugin's identifier, e.g. `"session-manager"`.
    pub plugin_name: String,
}

/// An interior template node, mirroring [`SplitNode`]: `children` and
/// `weights` are parallel, and `active_child_index` names the expanded member of a
/// [`SplitDirection::Stacked`] node (directional nodes carry it as zero).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateSplit {
    /// How the children divide this node's rectangle.
    pub direction: SplitDirection,
    /// The child subtrees, in layout order.
    pub children: Vec<TemplateNode>,
    /// Per-child size constraints, parallel to `children`.
    pub weights: Vec<SizeWeight>,
    /// Index of the expanded child. Only meaningful for `Stacked` nodes,
    /// where it names the one expanded member.
    pub active_child_index: usize,
}

/// A failed template instantiation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum TemplateError {
    /// The id list does not pair one id with each leaf.
    #[error(
        "template has {expected_leaf_count} pane slots but {provided_pane_id_count} pane ids were supplied"
    )]
    PaneCountMismatch {
        /// Leaf count of the template.
        expected_leaf_count: usize,
        /// Length of the supplied id slice.
        provided_pane_id_count: usize,
    },
}

impl DomainError for TemplateError {
    fn category(&self) -> DomainCategory {
        DomainCategory::Layout
    }

    fn get_severity(&self) -> Severity {
        Severity::Recoverable
    }
}
