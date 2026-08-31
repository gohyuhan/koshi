//! `koshi-daemon` — the koshi processes that run with no terminal of their
//! own.
//!
//! Three of them, each owning one thing and nothing else:
//!
//! - [`router`] — one per user. It owns the list of running sessions, starts
//!   and reaps one session server per session, and tells a caller where to
//!   reach one. No pane traffic passes through it.
//! - [`session_server`] — one per session. It owns that session's panes and
//!   their child processes, and answers that session's control socket.
//! - [`pty_supervisor`] — the process holding a session's panes on Windows,
//!   where a pseudoconsole cannot be handed to another process. It outlives a
//!   session server that replaces its own image.
//!
//! Every one of them is a subcommand of the `koshi` binary, started by the
//! process above it: a caller starts the router, the router starts a session
//! server, and a session server starts its supervisor. None of them is ever
//! typed by a person.
//!
//! What they share with the command-line program — reading the config files,
//! finding a running koshi, talking to it — is [`koshi_link`], below this
//! crate. What they serve on their sockets is [`koshi_ipc`].

/// Starting and replacing this crate's own processes: the signal mask a
/// serving thread runs under, replacing this image with another, and starting
/// a process that outlives its parent.
pub(crate) mod process;

/// The process holding one session's panes: it opens and closes every pane's
/// terminal, and outlives a session server that replaces its own image.
pub mod pty_supervisor;

/// The TLS port this machine serves remote clients on: the router opens it,
/// admits a caller by the secret it presents, and carries that caller's bytes
/// to and from one session server.
pub(crate) mod remote_listener;

/// The router process: it owns the list of running sessions, starts and
/// reaps one session server per session, and tells callers where to reach
/// them.
pub mod router;

/// The per-session server process: one session's panes and PTYs, served
/// headlessly over that session's control socket.
pub mod session_server;
