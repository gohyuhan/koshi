//! Logging configuration vocabulary shared by the config parser and the
//! logging subscriber.
//!
//! Both types are plain config values with no behavior of their own:
//! `koshi-config` parses them out of `koshi.kdl`, and `koshi-observability`
//! feeds them to the tracing subscriber. They live here so neither of those
//! crates has to depend on the other.

/// The lowest severity a log line must carry to be written. A line below the
/// configured level is dropped.
///
/// Example: with [`LogLevel::Warning`], a `tracing::warn!` and a
/// `tracing::error!` are written but a `tracing::info!` is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    /// Write everything: info, warning, and error lines.
    Info,
    /// Write warning and error lines; drop info.
    Warning,
    /// Write only error lines.
    Error,
}

/// How each written log line is rendered in the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    /// Human-readable, multi-line records for a person reading the file.
    Pretty,
    /// One JSON object per line, for a machine to parse.
    Json,
}
