//! Plugin host modules for instance state, command dispatch, and event
//! handling. The child modules define no items.

pub mod command;

pub mod event;

pub mod state;

#[cfg(test)]
mod tests;
