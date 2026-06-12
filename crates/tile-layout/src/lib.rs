//! `tile-layout` — pure layout engine: split tree, geometry solver, resize
//! transactions, pane removal cleanup, and layout normalization. No PTY or
//! rendering knowledge.

pub mod edit;
pub mod error;
pub mod focus;

pub mod layout;
pub mod mode;
pub mod normalize;
pub mod resize;
pub mod size;
pub mod solver;
pub mod tree;
