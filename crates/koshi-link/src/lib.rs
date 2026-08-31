//! `koshi-link` — what every koshi program needs before and around its real
//! job: read the config files, learn whether it is running inside a session,
//! reach a running koshi, and name what went wrong.
//!
//! Both kinds of koshi program sit on this crate. The command-line program
//! reads a config, finds the session a verb is aimed at, and submits it. A
//! server process reads the same config at startup and asks the router where
//! its neighbours are.
//!
//! Two halves, and nothing else:
//!
//! - **The files.** [`config`] reads `koshi.kdl`, the theme, the keybindings
//!   and the profiles off disk into the override layers and the admission
//!   policy `koshi-runtime` takes. Parsing them is [`koshi_config`]'s job;
//!   this half is the reading.
//! - **The sockets.** [`ipc_client`] talks to one session on its own control
//!   socket, [`router_client`] talks to the router on the router's, and
//!   [`talk`] holds the parts of an exchange that are the same for either
//!   peer. [`remote_client`] talks to another machine over TLS, on the
//!   address that machine listens on. [`discovery`] asks every session this
//!   machine advertises to describe itself and turns the answers into listing
//!   rows. [`in_session`] answers whether this program is running inside a
//!   pane, from the `KOSHI_*` variables the pane's shell was given.
//!
//! [`error`] is the failure both halves report, and the one a koshi program
//! turns into an exit code.

pub mod config;
pub mod discovery;
pub mod error;
pub mod in_session;
pub mod ipc_client;
pub mod remote_client;
pub mod router_client;
pub mod talk;
