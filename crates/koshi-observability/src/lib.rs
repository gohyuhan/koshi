//! `koshi-observability` — what koshi reports about itself.
//!
//! The crate holds the user-facing diagnostic messages, the tracing subscriber
//! that writes the per-session log file, the log line each committed runtime
//! event gets, and the terminal cleanup that runs on drop or panic and writes
//! a crash report.

/// User-facing diagnostic messages (config errors, command rejections, resize failures).
pub mod diagnostics;

/// Structured logging setup and canonical event fields.
pub mod logging;

/// Terminal cleanup hooks that survive panics.
pub mod cleanup;
