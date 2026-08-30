//! `koshi-runtime` — main orchestrator: the authoritative session half, event
//! loop machinery, command dispatcher, scheduler, runtime shutdown, and
//! cross-crate wiring.
//!
//! The [`server::Server`] owns all authoritative session state. One attached
//! terminal's view side lives in its own crate, `koshi-client`. The two halves
//! talk only through the server's doors: [`server::Server::submit_command`]
//! carries a command in, [`server::Server::subscribe`] carries the emitted
//! events out.
//!
//! [`ipc_server::IpcServer`] serves that session's control socket: it carries
//! every other process's request to the same dispatcher, and an attached
//! client's event stream back out. It sets each submitted command's source
//! from the connection the command arrived on. [`resume`] writes and reads the
//! file a session server hands to the process image that replaces it.

pub mod ipc_server;
pub mod resume;
pub mod runtime;
pub mod server;
