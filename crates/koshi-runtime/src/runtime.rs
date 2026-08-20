//! The server's internal machinery: local-session bootstrap (genesis), the
//! `KOSHI_*` environment a spawned pane's child gets, the loop-facing driver
//! surface, command dispatch, the event inbox, the event fan-out bus,
//! outer-input routing, mouse handling, highlight upkeep, clipboard writes,
//! PTY (pseudo-terminal, a child process's terminal connection) forwarding and
//! output handling, config reload transactions, render scheduling, the saved
//! views a detached client can take back, per-client scrollback scrolling,
//! staged shutdown, the render-snapshot builder, the wire-frame builder, the
//! attach-structure builder, the discovery-overview builder, the layout-dump
//! builder, and event transactions.
//! The [`Server`](crate::server::Server) type these modules extend lives in
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
pub mod layout;
pub mod mouse;
pub mod pty_forward;
pub mod pty_output;
pub mod reload;
pub mod render_schedule;
pub mod saved_view;
pub mod scroll;
pub mod selection;
pub mod shutdown;
pub mod snapshot;
pub(crate) mod spawn_env;
pub(crate) mod transaction;

#[cfg(test)]
mod tests;
