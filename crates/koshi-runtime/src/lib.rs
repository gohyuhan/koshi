//! `koshi-runtime` — main orchestrator: the authoritative session half, event
//! loop machinery, command dispatcher, scheduler, runtime shutdown, and
//! cross-crate wiring.
//!
//! The [`server::Server`] owns all authoritative session state. One attached
//! terminal's view side lives in its own crate, `koshi-client`. The two halves
//! talk only through the server's doors: [`server::Server::submit_command`]
//! carries a command in, [`server::Server::subscribe`] carries the emitted
//! events out.

pub mod error;
pub mod ipc_server;
pub mod placeholder;
pub mod runtime;
pub mod server;
pub mod types;
