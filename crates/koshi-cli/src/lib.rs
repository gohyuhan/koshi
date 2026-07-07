//! `koshi-cli` — binary entrypoint library: `clap` definitions, subcommands,
//! startup mode, and IPC client calls. Must not contain core runtime behavior.

/// Startup, the event loop, and terminal I/O for the interactive binary.
pub mod app;

/// Command-line grammar: root parser, attach/detach flags, subcommand tree.
pub mod cli;

/// CLI domain errors: unknown commands and invalid arguments.
pub mod error;

/// Keyboard event decoding: crossterm key events to child input bytes.
pub mod keys;
