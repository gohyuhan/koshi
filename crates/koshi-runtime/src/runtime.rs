//! The server's internal machinery: local-session bootstrap (genesis), the
//! loop-facing driver surface, command dispatch, the event inbox, the event
//! fan-out bus, outer-input routing, PTY (pseudo-terminal, a child process's
//! terminal connection) forwarding and output handling, config reload
//! transactions, render scheduling, per-client scrollback scrolling, staged
//! shutdown, the render-snapshot builder, the wire-frame builder, the
//! attach-structure builder, and event transactions. The
//! [`Server`](crate::server::Server) type these modules extend lives in
//! [`crate::server`].

pub mod attach;
pub mod bootstrap;
pub mod bus;
pub mod clipboard;
pub mod command;
pub mod discovery;
pub mod driver;
pub mod event;
pub mod frame;
pub mod input;
pub mod mouse;
pub mod pty_forward;
pub mod pty_output;
pub mod reload;
pub mod render_schedule;
pub mod scroll;
pub mod selection;
pub mod shutdown;
pub mod snapshot;
pub(crate) mod spawn_env;
pub(crate) mod transaction;

#[cfg(test)]
mod tests;
