//! Layout modes that change how a tree is solved without changing the tree.
//!
//! Fullscreen (zoom) is the only mode beyond plain tiling: one pane takes
//! the whole tab rect and every other pane is hidden. The mode is a value
//! stored beside the tree; no mode edits the tree. Leaving fullscreen
//! restores the exact prior layout.
//!
//! Each client holds its own mode. The solver takes the mode as an argument
//! ([`crate::solver::solve_with_mode_min`]). Two clients can solve the same tree
//! in the same frame, one fullscreen and one tiled.

use koshi_core::ids::PaneId;
use serde::{Deserialize, Serialize};

/// How one client's view of a layout tree is solved.
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
