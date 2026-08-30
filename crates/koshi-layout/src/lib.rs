//! `koshi-layout` — the pane layout engine. No PTY (pseudo-terminal process)
//! and no rendering knowledge.
//!
//! It holds the split tree and its size constraints, the geometry solver that
//! turns a tree plus a tab rectangle into pane rectangles, the structural
//! edits (split, stack, remove), resize transactions, normalization after an
//! edit, the fullscreen mode, focus candidates after a pane closes, per-pane
//! content rectangles, and layout templates that describe an arrangement
//! before any pane exists.

pub mod content;
pub mod edit;
pub mod focus;
pub mod mode;
pub mod normalize;
pub mod regions;
pub mod resize;
pub mod size;
pub mod solver;
pub mod template;
#[cfg(test)]
mod test_trees;
pub mod tree;
