//! `tile-cli` — binary entrypoint library: `clap` definitions, subcommands,
//! startup mode, and IPC client calls. Must not contain core runtime behavior.

/// CLI domain errors: unknown commands and invalid arguments.
pub mod error;

/// Subcommands.
pub mod subcommand;
