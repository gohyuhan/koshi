//! Core runtime components: command dispatch, the event inbox, PTY output
//! routing, render scheduling, event transactions, and the runtime state
//! container.

pub mod command;
pub mod event;
pub mod pty_output;
pub mod render_schedule;
pub mod state;
pub mod transaction;

#[cfg(test)]
mod tests;
