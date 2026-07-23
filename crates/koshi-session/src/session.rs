//! `session` domain — skeleton per standard source layout.

pub mod cascade;
pub mod command;
pub mod event;
pub mod focus;
pub mod lifecycle;
pub mod pane_ops;
pub mod policy;
pub mod state;
pub mod tab_ops;

#[cfg(test)]
mod tests;
