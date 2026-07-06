//! Tab-level layout modes that change how the tree is solved without
//! changing the tree.
//!
//! Fullscreen (zoom) is the only mode beyond plain tiling: one pane is
//! promoted to the whole tab rect and everything else is hidden. The split
//! tree is never rewritten for it — the mode is a sidecar value, so leaving
//! fullscreen restores the exact prior layout by construction. There is
//! nothing to put back and nothing that can drift.
//!
//! Content replacement (swapping what a leaf shows) is a one-shot tree edit
//! with unchanged geometry, handled with the other edits.

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
