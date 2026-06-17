//! `tile-pty` — process/PTY backend: `portable-pty` wrapper, shell bootstrap,
//! PTY read/write/resize, and child process exit detection.

pub mod error;
pub mod portable;
pub mod types;

pub mod backend;
