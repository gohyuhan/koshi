//! `koshi-pty` — runs a program in a pseudo-terminal and drives it: the
//! `portable-pty` wrapper, the environment overlay a shell starts with, PTY
//! read/write/resize, child termination, and exit detection. Panes held by
//! another process are driven the same way over the supervisor link.
//!
//! A PTY (pseudo-terminal) is an OS-level pair of linked file handles. A
//! spawned program (a shell, for example) attached to one behaves as if it
//! were talking to a real terminal: line editing and colors work.

/// OS lookups about working directories and the machine's own name.
pub mod cwd;

/// The environment overlay a spawned child starts with.
mod env;

/// Error types for PTY operations.
pub mod error;

/// Process termination and kill signal operations.
pub mod kill;

/// `portable-pty` wrapper and abstractions.
pub mod portable;

/// PTY resize operations.
pub mod resize;

/// The `PtyBackend` trait and the `PtyHandle` a spawned pane is driven
/// through; the concrete backend built on `portable-pty` lives in [`portable`].
pub mod backend;

/// Driving panes that live in another process, over the supervisor link.
pub mod supervisor;
