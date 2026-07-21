//! `koshi-runtime` — main orchestrator: the in-process server/client ownership
//! split, event loop machinery, command dispatcher, scheduler, runtime
//! shutdown, and cross-crate wiring.
//!
//! The [`server::Server`] owns all authoritative session state; the
//! [`client::Client`] owns one attached terminal's view side. They talk only
//! through the server's doors — [`server::Server::submit_command`] and
//! [`server::Server::subscribe`] — so the halves can move to separate
//! processes without redrawing the ownership boundary.

pub mod client;
pub mod error;
pub mod placeholder;
pub mod runtime;
pub mod server;
pub mod types;
