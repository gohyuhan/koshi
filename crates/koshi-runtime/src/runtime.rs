//! Core runtime components: command dispatch, the event inbox, PTY output
//! routing, render scheduling, the render-snapshot builder, event
//! transactions, and the runtime state container.

pub mod bootstrap;
pub mod command;
pub mod driver;
pub mod event;
pub mod input;
pub mod pty_forward;
pub mod pty_output;
pub mod render_schedule;
pub mod scroll;
pub mod snapshot;
pub mod state;
pub mod transaction;

#[cfg(test)]
mod tests;
