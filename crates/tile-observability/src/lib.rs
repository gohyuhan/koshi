//! `tile-observability` — diagnostics: tracing setup, debug dumps, crash context,
//! and event logs.

/// User-facing diagnostic messages (config errors, command rejections, resize failures).
pub mod diagnostics;

/// Structured logging setup and canonical event fields.
pub mod logging;

/// Terminal cleanup hooks that survive panics.
pub mod cleanup;
