//! `koshi` — the command-line program: the grammar a person types, the
//! startup mode it picks, and the answers it prints.
//!
//! What it owns is the surface a person touches. Everything behind that
//! surface belongs to a crate below it, and this one calls into them:
//!
//! - [`koshi_link`] reads the config files, finds a running koshi, and talks
//!   to it. Shared with every other koshi program.
//! - [`koshi_daemon`] holds the three processes that run with no terminal —
//!   the router, a session server, and a pane supervisor. the binary starts one
//!   of them when it is given that subcommand, and never otherwise.
//! - [`koshi_client`] holds the viewer: one attached terminal's own state, the
//!   loop that drives it, and the terminal I/O under it.
//!
//! So a verb typed at a shell is parsed here, resolved to a target here, sent
//! through `koshi-link`, and printed here. Nothing in this crate serves a
//! socket or owns a pane.

/// Command-line grammar: root parser, root flags, subcommand tree.
pub mod cli;

/// Local config path, explanation, validation, and migration commands.
pub mod config_command;

/// The offline keymap view served by the `koshi keys` queries: the user's
/// keybinding file folded onto the built-in defaults, conflict-checked and
/// merged.
pub mod keymap;

/// Table and JSON rendering for discovery query answers, action-registry
/// introspection, keymap introspection, the `debug` dumps, and the version
/// answers.
pub mod output;

/// Process-level session commands that work without an attached pane.
pub mod session_control;

/// The `koshi share` commands: grant, revoke and list the remote access
/// tokens this machine has handed out.
pub mod share;

/// Which running session a command goes to: in-session fast path, explicit
/// `--session`/`--tab`/`--pane`/`--client` targets, and the count rule.
pub mod targeting;

/// Self-update: check GitHub for a newer koshi release and install it.
pub mod updater;

/// Which koshi build is running: this program, and each koshi server.
pub mod version;
