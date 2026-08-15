//! `koshi-link` — what every koshi program needs before and around its real
//! job: read the config files, learn whether it is running inside a session,
//! reach a running koshi, and name what went wrong.
//!
//! Both kinds of koshi program sit on this crate. The command-line program
//! reads a config, finds the session a verb is aimed at, and submits it. A
//! server process reads the same config at startup and asks the router where
//! its neighbours are. Neither owns this ground, so it sits under both.
//!
//! It sits *over* the runtime, not under it: reading a config file means
//! producing the override layers and the admission policy `koshi-runtime`
//! takes, so this crate names those types rather than the other way round.
//!
//! Two halves, and nothing else:
//!
//! - **The files.** [`config`] reads `koshi.kdl`, the theme, the keybindings
//!   and the profiles off disk into the override layers the runtime takes.
//!   Parsing them is [`koshi_config`]'s job; this half is the reading.
//! - **The sockets.** [`ipc_client`] talks to one session on its own control
//!   socket, [`router_client`] talks to the router on the router's, and
//!   [`talk`] holds the parts of an exchange that are the same for either
//!   peer. [`in_session`] answers whether this program is running inside a
//!   pane, from the `KOSHI_*` variables the pane's shell was given.
//!
//! [`error`] is the failure both halves report, and the one a koshi program
//! turns into an exit code.

/// Reading the config files at startup into override layers for the runtime.
pub mod config;

/// Answering the discovery queries across every running koshi: probe each
/// advertised session, sweep the ones that are gone, build listing rows.
pub mod discovery;

/// Failures a koshi program reports: unknown commands, invalid arguments, and
/// a running koshi that could not be reached.
pub mod error;

/// In-session detection: the `KOSHI_*` identity variables read at startup.
pub mod in_session;

/// The client side of a session's control socket: connect to a session's
/// advertised endpoint, submit a command, and read back its result.
pub mod ipc_client;

/// The client side of the router socket: ask the router something, starting
/// one first when none is running.
pub mod router_client;

/// What every one-shot exchange with a running koshi does the same way, for
/// either peer: settle the version, unwrap the answer, name the failure.
pub mod talk;
