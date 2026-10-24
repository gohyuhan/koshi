//! `koshi-observability` — what koshi reports about itself.
//!
//! The crate holds the tracing subscriber that writes the session and router
//! log files, the log line each committed runtime event gets, the ring of
//! recent events `koshi debug events` prints, the limit on how often one
//! repeated warning is written, and the terminal cleanup that runs on drop or
//! panic and writes a crash report.

/// Structured logging setup for the session and router log files.
pub mod logging;

/// One repeated warning, written at most once a minute.
pub mod repeated_warning;

/// Terminal cleanup hooks that survive panics.
pub mod cleanup;
