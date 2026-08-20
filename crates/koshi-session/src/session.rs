//! The session domain modules: state, lifecycle, tabs, panes, focus, and the
//! removal cascade.

pub mod cascade;
pub mod focus;
pub mod lifecycle;
pub mod pane_ops;
pub mod policy;
pub mod state;
pub mod tab_ops;

#[cfg(test)]
mod tests;
