//! `logging` domain — structured logging bootstrap.
//!
//! [`init_tracing`] installs a process-wide subscriber that emits structured
//! logs, formatted as JSON or human-readable text per [`TracingOptions`]. Logs
//! carry a fixed set of [canonical fields](self#canonical-fields) so a session
//! can be followed across panes, commands, and plugins.
//!
//! Redaction is not optional: anything derived from the environment must pass
//! through [`redacted_env_field`] before it becomes a log value, so a secret
//! such as `TILE_CONTEXT_TOKEN` can never reach the output even if it is handed
//! to the logger by mistake. The scrubbing itself lives in [`tile_core::redact`];
//! this module only routes env maps through it on the way to a log line.
//!
//! Environment variables read by [`TracingOptions::from_env`]:
//! - `TILE_LOG_FORMAT` — `json` or `pretty` (default: `pretty`).
//! - `TILE_LOG` — tracing filter directive, e.g. `info` or `tile=debug`
//!   (default: `info`).

use std::collections::BTreeMap;
use std::io;
use std::sync::{Arc, Mutex};

use thiserror::Error;
use tracing_subscriber::fmt;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::EnvFilter;

use tile_core::redact::redact_env_map;

/// The canonical field names every cross-cutting log line should carry. They are
/// the join keys for tracing one session across panes, commands, and plugins.
pub const CANONICAL_FIELDS: [&str; 8] = [
    "session_id",
    "client_id",
    "tab_id",
    "pane_id",
    "command_id",
    "event_id",
    "plugin_id",
    "subscriber_id",
];

/// How log lines are rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    /// One JSON object per line, for machine ingestion.
    Json,
    /// Human-readable multi-line records, for a developer at a terminal.
    Pretty,
}

impl LogFormat {
    /// Read the format from `TILE_LOG_FORMAT` (`json` or `pretty`). Anything else,
    /// including an unset variable, falls back to [`LogFormat::Pretty`].
    pub fn from_env() -> Self {
        LogFormat::parse(std::env::var("TILE_LOG_FORMAT").ok().as_deref())
    }

    /// The pure mapping behind [`from_env`]: `Some("json")` is JSON, anything else
    /// (including `None`) is pretty. Kept separate so it can be tested without
    /// touching the process-global environment.
    pub fn parse(value: Option<&str>) -> Self {
        match value {
            Some("json") => LogFormat::Json,
            _ => LogFormat::Pretty,
        }
    }
}

/// Knobs for [`init_tracing`].
#[derive(Debug, Clone)]
pub struct TracingOptions {
    /// How log lines are rendered.
    pub format: LogFormat,
    /// A `tracing_subscriber` env-filter directive (e.g. `info`, `tile=debug`).
    pub filter: String,
}

impl TracingOptions {
    /// Build options from the environment: [`LogFormat::from_env`] for the format
    /// and `TILE_LOG` (defaulting to `info`) for the filter.
    pub fn from_env() -> Self {
        TracingOptions {
            format: LogFormat::from_env(),
            filter: std::env::var("TILE_LOG").unwrap_or_else(|_| "info".to_string()),
        }
    }
}

impl Default for TracingOptions {
    fn default() -> Self {
        TracingOptions::from_env()
    }
}

/// Why [`init_tracing`] could not install a subscriber.
#[derive(Debug, Error)]
pub enum TracingError {
    /// The filter directive failed to parse.
    #[error("invalid log filter: {0}")]
    Filter(String),
    /// A global subscriber was already installed for this process.
    #[error("tracing is already initialized for this process")]
    AlreadyInitialized,
}

/// Holds resources tied to the active subscriber. Keep it alive for as long as
/// logging is needed.
///
/// Dropping it is a no-op today: the global subscriber outlives it. The type
/// exists so a future non-blocking writer can flush on drop without changing
/// [`init_tracing`]'s signature.
#[derive(Debug)]
pub struct TracingGuard {
    _private: (),
}

/// Install the process-wide tracing subscriber.
///
/// Returns [`TracingError::AlreadyInitialized`] if called more than once, since a
/// process has a single global subscriber.
pub fn init_tracing(opts: TracingOptions) -> Result<TracingGuard, TracingError> {
    let filter =
        EnvFilter::try_new(&opts.filter).map_err(|err| TracingError::Filter(err.to_string()))?;

    let result = match opts.format {
        LogFormat::Json => fmt().with_env_filter(filter).json().try_init(),
        LogFormat::Pretty => fmt().with_env_filter(filter).pretty().try_init(),
    };
    result.map_err(|_| TracingError::AlreadyInitialized)?;

    Ok(TracingGuard { _private: () })
}

/// A thread-local capture of log output. Returned by [`with_test_writer`] so a
/// test can assert on what was logged.
#[derive(Clone, Default)]
pub struct CapturedLogs {
    buffer: Arc<Mutex<Vec<u8>>>,
}

impl CapturedLogs {
    /// All captured output as a single string. Recovers a poisoned lock rather
    /// than panicking: if a writer thread panicked mid-log, the bytes it already
    /// wrote are still readable, so reading them must not cascade into a second
    /// panic.
    pub fn contents(&self) -> String {
        let bytes = self
            .buffer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        String::from_utf8_lossy(&bytes).into_owned()
    }

    /// The captured output split into lines (one JSON record per line).
    pub fn lines(&self) -> Vec<String> {
        self.contents().lines().map(str::to_owned).collect()
    }
}

/// The `io::Write` end of a [`CapturedLogs`] buffer, handed to the fmt layer.
pub struct CapturedWriter {
    buffer: Arc<Mutex<Vec<u8>>>,
}

impl io::Write for CapturedWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.buffer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for CapturedLogs {
    type Writer = CapturedWriter;

    fn make_writer(&'a self) -> Self::Writer {
        CapturedWriter {
            buffer: Arc::clone(&self.buffer),
        }
    }
}

/// Install a JSON subscriber scoped to the current thread and capture its output.
///
/// The returned guard scopes the subscriber to the calling thread, so tests stay
/// isolated from one another and from any global subscriber. Drop the guard to
/// restore the previous subscriber; read the [`CapturedLogs`] to assert on output.
pub fn with_test_writer() -> (tracing::subscriber::DefaultGuard, CapturedLogs) {
    let logs = CapturedLogs::default();
    let subscriber = fmt()
        .with_env_filter(EnvFilter::new("trace"))
        .json()
        .with_writer(logs.clone())
        .finish();
    let guard = tracing::subscriber::set_default(subscriber);
    (guard, logs)
}

/// Redact an environment map and render it as a single log-safe field value of
/// space-separated `KEY=value` pairs. Sensitive values (per [`tile_core::redact`])
/// render as `***`. Use this for any env-derived value before logging it.
pub fn redacted_env_field(env: &BTreeMap<String, String>) -> String {
    redact_env_map(env)
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests;
