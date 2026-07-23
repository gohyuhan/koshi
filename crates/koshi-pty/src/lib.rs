//! `koshi-pty` — process/PTY backend: `portable-pty` wrapper, shell bootstrap,
//! PTY read/write/resize, and child process exit detection.
//!
//! A PTY (pseudo-terminal) is an OS-level pair of linked file handles that
//! makes a spawned program (a shell, for example) behave as if it were
//! talking to a real terminal, so interactive behavior like line editing and
//! colors works when koshi runs it.

/// OS lookups about working directories and the machine's own name.
pub mod cwd;
mod env;

/// Error types for PTY operations.
pub mod error;

/// Process termination and kill signal operations.
pub mod kill;

/// `portable-pty` wrapper and abstractions.
pub mod portable;

/// PTY resize operations.
pub mod resize;

/// Shared type definitions.
pub mod types;

/// The `PtyBackend` trait and the `PtyHandle` a spawned pane is driven
/// through; the concrete backend built on `portable-pty` lives in [`portable`].
pub mod backend;
