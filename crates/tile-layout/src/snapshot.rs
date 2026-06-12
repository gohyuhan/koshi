//! Persistence DTOs for layout state that outlives a process.
//!
//! A stack's identity is its membership, its active member, and which
//! members are collapsed. That is exactly what survives a detach/attach or
//! a daemon restart — live PTY state does not. The snapshot stores pane ids
//! and flags only; weights are not part of it because collapsed members
//! have no independent size and the active member takes whatever the stack
//! gets.

use serde::{Deserialize, Serialize};
use tile_core::geometry::SplitDirection;
use tile_core::ids::PaneId;

use crate::tree::SplitNode;

/// A stack's persisted shape: who is in it, who is expanded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackSnapshot {
    /// Member panes in stack order.
    pub members: Vec<PaneId>,
    /// Index of the expanded member.
    pub active: usize,
    /// Per-member collapsed flags, parallel to `members`. Stored explicitly
    /// rather than derived from `active`, so a snapshot restores exactly
    /// the flags it captured.
    pub collapsed_states: Vec<bool>,
}

impl StackSnapshot {
    /// Capture a stack's persisted shape. `None` when `node` is not a
    /// stack. A member that is itself a subtree is represented by its
    /// first pane — a shape the edits never produce. A member without any
    /// pane is dropped, and the active index follows its member through
    /// that filtering; when the active member itself is dropped, the last
    /// member stands in.
    #[must_use]
    pub fn capture(stack: &SplitNode) -> Option<Self> {
        if stack.direction != SplitDirection::Stacked {
            return None;
        }
        let source_active = stack.active.min(stack.children.len().saturating_sub(1));
        let mut members = Vec::with_capacity(stack.children.len());
        let mut collapsed_states = Vec::with_capacity(stack.children.len());
        let mut active = None;
        for (index, child) in stack.children.iter().enumerate() {
            let Some(&pane) = child.node.leaf_panes().first() else {
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
    /// clamped into bounds and the collapsed flags are re-paired with the
    /// members, so a snapshot from an older or damaged store still restores
    /// to a usable stack.
    #[must_use]
    pub fn restore(&self) -> SplitNode {
        let mut stack = SplitNode::stack(self.members.clone(), self.active);
        for (index, child) in stack.children.iter_mut().enumerate() {
            if let Some(&collapsed) = self.collapsed_states.get(index) {
                child.collapsed = collapsed;
            }
        }
        stack
    }
}

#[cfg(test)]
mod tests;
