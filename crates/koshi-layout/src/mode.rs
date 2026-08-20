//! Layout modes that change how a tree is solved without changing the tree.
//!
//! Fullscreen (zoom) is the only mode beyond plain tiling: one pane is
//! promoted to the whole tab rect and everything else is hidden. The mode is
//! a value beside the tree, never a rewrite of it. Leaving fullscreen
//! restores the exact prior layout.
//!
//! **A mode belongs to a viewer, not to a tab.** The solver takes one as an
//! argument and never reads it off the tab. Two clients can solve the same
//! tree in the same frame, one zoomed and one tiled. The session layer stores
//! the mode per client.

use koshi_core::ids::PaneId;
use serde::{Deserialize, Serialize};

/// How a tab's layout tree is currently being solved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LayoutMode {
    /// The tree solves normally: every pane gets its tiled rect.
    Tiled,
    /// `focused` fills the whole tab; all other panes solve to zero area.
    /// The underlying tree keeps its exact shape, including stack
    /// membership and active children.
    Fullscreen {
        /// The promoted pane.
        focused: PaneId,
    },
}

#[cfg(test)]
mod tests;
