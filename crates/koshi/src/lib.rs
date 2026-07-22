//! `koshi` — binary entrypoint library: `clap` definitions, subcommands,
//! startup mode, and IPC client calls. Must not contain core runtime behavior.

/// Startup, the event loop, and terminal I/O for the interactive binary.
pub mod app;

/// Command-line grammar: root parser, attach/detach flags, subcommand tree.
pub mod cli;

/// Reading the config files at startup into override layers for the runtime.
pub mod config;

/// CLI domain errors: unknown commands and invalid arguments.
pub mod error;

/// In-session detection: the `KOSHI_*` identity variables read at startup.
pub mod in_session;

/// Keyboard event decoding: crossterm key events to child input bytes.
pub mod keys;

/// The offline keymap view served by the `koshi keys` queries: the user's
/// keybinding file folded onto the built-in defaults, conflict-checked and
/// merged.
pub mod keymap;

/// Table and JSON rendering for discovery query answers, action-registry
/// introspection, and keymap introspection.
pub mod output;

/// Self-update: check GitHub for a newer koshi release and install it.
pub mod updater;
