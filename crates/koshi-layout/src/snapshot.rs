//! Persistence data types for layout state that outlives a process: plain
//! structs serialized to disk and read back, separate from the live tree
//! types the solver works on.
//!
//! A stack's persisted shape is its member pane ids, its active member, and
//! the collapsed flag of each member. It stores no weights.

use koshi_core::geometry::SplitDirection;
use koshi_core::ids::PaneId;
use serde::{Deserialize, Serialize};

use crate::tree::SplitNode;

/// A stack's persisted shape: who is in it, who is expanded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackSnapshot {
    /// Member panes in stack order.
    pub members: Vec<PaneId>,
    /// Index of the expanded member.
    pub active: usize,
    /// Per-member collapsed flags, parallel to `members`, exactly as
    /// captured. [`StackSnapshot::restore`] applies them as stored. A stored
    /// shape without the field reads as an empty list, and `restore` then
    /// derives every flag from `active`.
    #[serde(default)]
    pub collapsed_states: Vec<bool>,
}

impl StackSnapshot {
    /// Capture a stack's persisted shape. `None` when `stack` is not a
    /// stack. A member that is itself a subtree is represented by its first
    /// pane. A member without any pane is dropped, and the active index
    /// follows its member through that filtering; when the active member
    /// itself is dropped, the last member stands in.
    #[must_use]
    pub fn capture(stack: &SplitNode) -> Option<Self> {
        if stack.direction != SplitDirection::Stacked {
            return None;
        }
        let source_active = stack.active_index();
        let mut members = Vec::with_capacity(stack.children.len());
        let mut collapsed_states = Vec::with_capacity(stack.children.len());
        let mut active = None;
        for (index, child) in stack.children.iter().enumerate() {
            let Some(pane) = child.node.first_leaf() else {
                continue;
            };
            if index == source_active {
                active = Some(members.len());
            }
            members.push(pane);
            collapsed_states.push(child.collapsed);
        }
        let active = active.unwrap_or(members.len().saturating_sub(1));
        Some(Self {
            members,
            active,
            collapsed_states,
        })
    }

    /// Rebuild the stack this snapshot describes. The active index is
    /// clamped into bounds by [`SplitNode::stack`]. `collapsed_states` is
    /// applied by index: a member past its end keeps the flag derived from
    /// `active`, and flags past the member count are ignored.
    #[must_use]
    pub fn restore(&self) -> SplitNode {
        let mut stack = SplitNode::stack(self.members.clone(), self.active);
        for (child, &collapsed) in stack.children.iter_mut().zip(&self.collapsed_states) {
            child.collapsed = collapsed;
        }
        stack
    }
}

#[cfg(test)]
mod tests;
