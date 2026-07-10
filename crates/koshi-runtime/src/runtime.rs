//! Core runtime components: local-session bootstrap (genesis), the
//! loop-facing driver surface, command dispatch, the event inbox, outer-input
//! routing, PTY (pseudo-terminal, a child process's terminal connection)
//! forwarding and output handling, render scheduling, per-client scrollback
//! scrolling, staged shutdown, the render-snapshot builder, event
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
pub mod shutdown;
pub mod snapshot;
pub mod state;
pub mod transaction;

#[cfg(test)]
mod tests;
