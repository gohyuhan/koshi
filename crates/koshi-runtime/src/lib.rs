//! `koshi-runtime` — main orchestrator: the authoritative session half, event
//! loop machinery, command dispatcher, scheduler, runtime shutdown, and
//! cross-crate wiring.
//!
//! The [`server::Server`] owns all authoritative session state. One attached
//! terminal's view side lives in its own crate, `koshi-client`, and the two
//! talk only through the server's doors — [`server::Server::submit_command`]
//! and [`server::Server::subscribe`] — so the halves can move to separate
//! processes without redrawing the ownership boundary.

pub mod error;
pub mod ipc_server;
pub mod placeholder;
pub mod runtime;
pub mod server;
pub mod types;
