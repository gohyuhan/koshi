//! `koshi` — binary entrypoint library: `clap` definitions, subcommands,
//! startup mode, and IPC client calls. Must not contain core runtime behavior.

/// The bare `koshi` launch, and the terminal I/O every attached client uses.
pub mod app;

/// The attached client: join a running session over its control socket and
/// read its event stream. A switch re-attaches the same terminal to the named
/// session; a detach, the session ending, or a broken connection ends the
/// client.
pub mod attach;

/// Command-line grammar: root parser, root flags, subcommand tree.
pub mod cli;

/// Reading the config files at startup into override layers for the runtime.
pub mod config;

/// Local config path, explanation, validation, and migration commands.
pub mod config_command;

/// Answering the discovery queries across every running koshi: probe each
/// advertised session, sweep the ones that are gone, build listing rows.
pub mod discovery;

/// CLI domain errors: unknown commands and invalid arguments.
pub mod error;

/// In-session detection: the `KOSHI_*` identity variables read at startup.
pub mod in_session;

/// The CLI side of the control socket: connect to a session's advertised
/// endpoint, submit a command, and read back its result.
pub mod ipc_client;

/// The process holding one session's panes: it opens and closes every pane's
/// terminal, and outlives a session server that replaces its own image.
pub mod pty_supervisor;

/// The router process: it owns the list of running sessions, starts and
/// reaps one session server per session, and tells callers where to reach
/// them.
pub mod router;

/// The client side of the router socket: ask the router something, starting
/// one first when none is running.
pub mod router_client;

/// Process-level session commands that work without an attached pane.
pub mod session_control;

/// The per-session server process: one session's panes and PTYs, served
/// headlessly over that session's control socket.
pub mod session_server;

/// Keyboard event decoding: crossterm key events to child input bytes.
pub mod keys;

/// Which running session a command goes to: in-session fast path, explicit
/// `--session`/`--tab`/`--pane`/`--client` targets, and the count rule.
pub mod targeting;

/// The offline keymap view served by the `koshi keys` queries: the user's
/// keybinding file folded onto the built-in defaults, conflict-checked and
/// merged.
pub mod keymap;

/// Table and JSON rendering for discovery query answers, action-registry
/// introspection, keymap introspection, and the `debug` dumps.
pub mod output;

/// Self-update: check GitHub for a newer koshi release and install it.
pub mod updater;
