//! `tile-pty` — process/PTY backend: `portable-pty` wrapper, shell bootstrap,
//! PTY read/write/resize, and child process exit detection.

mod env;
pub mod error;
pub mod kill;
pub mod portable;
pub mod types;

pub mod backend;
