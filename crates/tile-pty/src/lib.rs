//! `tile-pty` — process/PTY backend: `portable-pty` wrapper, shell bootstrap,
//! PTY read/write/resize, and child process exit detection.

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

/// Main PTY backend implementation and state management.
pub mod backend;
