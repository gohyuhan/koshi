//! `logging` domain — the tracing subscriber that writes koshi's log file.
//!
//! Every `tracing::info!` / `warn!` / `error!` call anywhere in the workspace
//! routes to the one process-wide subscriber [`logging::init_tracing`] installs. That
//! subscriber is the single place three questions are answered, all from the
//! `logging` section of `koshi.kdl` — nothing is read from the environment:
//!
//! - **Should this line be written?** [`logging::LoggingParams::enabled`] — disabled
//!   installs no subscriber at all, so no line is written and no file or
//!   `logs/` directory is ever created.
//! - **Where does it go?** A per-session file `logs/koshi-log-<id>.log` under
//!   the user's state directory (see [`logging::session_log_path`]). The file is
//!   created on the *first* line written and re-created if it is removed while
//!   koshi runs.
//! - **What passes the bar?** [`logging::LoggingParams::level`] — the lowest severity
//!   that gets written; a line below it is dropped before it reaches the file.
//!
//! # Logging policy
//!
//! Logs record **errors** and **domain events** — nothing else. They are a trail
//! of *what happened and what triggered it*, not a narration of *what the code
//! was doing*. Each line should carry only the minimum needed to correlate it
//! back to its cause: the [canonical IDs](self#canonical-fields) plus an event or
//! error kind. No payloads, no command arguments, no terminal/PTY output, no
//! per-frame or per-keystroke activity. Anything high-frequency or content-like
//! belongs in the in-memory event ring (`koshi debug events`), not the log file.
//! This keeps the file small over a session and free of user data — and keeps
//! the per-line file open cheap, since the volume stays low.
//!
//! When enabled, logs never go to stdout: that is Koshi's render surface, and
//! writing to it would corrupt the terminal UI.
//!
//! Redaction is not optional: anything derived from the environment must pass
//! through [`logging::redacted_env_field`] before it becomes a log value, so a secret
//! such as `KOSHI_CONTEXT_TOKEN` can never reach the output even if it is handed
//! to the logger by mistake. The scrubbing itself lives in [`koshi_core::redact`];
//! this module only routes env maps through it on the way to a log line.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use thiserror::Error;
use tracing::Level;
use tracing_subscriber::fmt;
use tracing_subscriber::fmt::MakeWriter;

use koshi_core::ids::SessionId;
use koshi_core::log::{LogFormat, LogLevel};
use koshi_core::redact::redact_env_map;

/// The canonical field names every cross-cutting log line should carry. They are
/// correlation IDs — the join keys for tracing one event back to its cause across
/// panes, commands, and plugins — not descriptions of state or activity.
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

/// Everything the subscriber needs, resolved from the `logging` config section.
#[derive(Debug, Clone)]
pub struct LoggingParams {
    /// Whether to install a subscriber and write a file at all.
    pub enabled: bool,
    /// The lowest severity that gets written.
    pub level: LogLevel,
    /// How each written line is rendered.
    pub format: LogFormat,
    /// The session this run logs under; names the per-session log file.
    pub session_id: SessionId,
}

/// The log file for `session_id`: `logs/koshi-log-<uuid>.log` under the user's
/// state directory (resolved by [`koshi_paths::state_dir`]) —
/// `~/.local/state/koshi/logs` on Linux, `~/Library/Application
/// Support/koshi/logs` on macOS, `%LOCALAPPDATA%\koshi\data\logs` on Windows.
/// If no home directory can be found at all, the file lands in the current
/// directory as a last resort.
///
/// Example: session `…446655440000` resolves on Linux to
/// `~/.local/state/koshi/logs/koshi-log-…446655440000.log`.
#[must_use]
pub fn session_log_path(session_id: SessionId) -> PathBuf {
    let name = format!("koshi-log-{}.log", session_id.as_uuid());
    match koshi_paths::state_dir() {
        Some(dir) => dir.join("logs").join(name),
        None => PathBuf::from(name),
    }
}

/// Why [`init_tracing`] could not install a subscriber.
#[derive(Debug, Error)]
pub enum TracingError {
    /// A global subscriber was already installed for this process.
    #[error("tracing is already initialized for this process")]
    AlreadyInitialized,
}

/// Install the process-wide tracing subscriber from resolved config.
///
/// Disabled installs nothing and touches no disk: with no global subscriber,
/// every event is dropped and no file or directory is created. Enabled installs
/// a subscriber that writes the per-session file lazily on the first line.
///
/// Returns [`TracingError::AlreadyInitialized`] if a subscriber is already
/// installed, since a process has a single global subscriber.
pub fn init_tracing(params: LoggingParams) -> Result<(), TracingError> {
    if !params.enabled {
        return Ok(());
    }
    init_to_path(
        &session_log_path(params.session_id),
        params.level,
        params.format,
    )
}

/// Install a subscriber writing to `path`. Separated from [`init_tracing`] so a
/// test can point it at a temp directory without going through the
/// state-directory resolver.
pub fn init_to_path(path: &Path, level: LogLevel, format: LogFormat) -> Result<(), TracingError> {
    let writer = SessionLogMaker {
        path: path.to_path_buf(),
    };
    // `with_ansi(false)`: the file is plain text, never a color terminal. The
    // format method (`pretty`/`json`) is the only thing that differs per arm.
    let builder = fmt()
        .with_max_level(max_level(level))
        .with_ansi(false)
        .with_writer(writer);
    let result = match format {
        LogFormat::Pretty => builder.pretty().try_init(),
        LogFormat::Json => builder.json().try_init(),
    };
    result.map_err(|_| TracingError::AlreadyInitialized)
}

/// The most verbose severity that still gets written for a configured level:
/// `warning` admits warnings and errors, `error` admits only errors.
fn max_level(level: LogLevel) -> Level {
    match level {
        LogLevel::Info => Level::INFO,
        LogLevel::Warning => Level::WARN,
        LogLevel::Error => Level::ERROR,
    }
}

/// A [`MakeWriter`] that appends each formatted event to a per-session log
/// file, creating the file — and its `logs/` parent — on the first write and
/// re-creating it if it is removed while koshi runs. The [logging
/// policy](self#logging-policy) keeps the volume low (errors and domain events
/// only), so opening the file per event is not a hot path.
// ponytail: reopen-per-event is fine at these volumes; cache the handle if logging ever gets chatty.
struct SessionLogMaker {
    path: PathBuf,
}

impl<'a> MakeWriter<'a> for SessionLogMaker {
    type Writer = SessionLogWriter;

    fn make_writer(&'a self) -> Self::Writer {
        SessionLogWriter {
            path: self.path.clone(),
        }
    }
}

/// The `io::Write` half of [`SessionLogMaker`]: opens the file in
/// create-and-append mode for one event's bytes, then drops it (which flushes
/// and closes it), so every written line is on disk before the next event.
struct SessionLogWriter {
    path: PathBuf,
}

impl io::Write for SessionLogWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?
            .write_all(buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
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
        .with_max_level(Level::TRACE)
        .json()
        .with_writer(logs.clone())
        .finish();
    let guard = tracing::subscriber::set_default(subscriber);
    (guard, logs)
}

/// Redact an environment map and render it as a single log-safe field value of
/// space-separated `KEY=value` pairs. Sensitive values (per [`koshi_core::redact`])
/// render as `***`. Use this for any env-derived value before logging it.
///
/// Environment is the one payload the [logging policy](self#logging-policy)
/// admits — it is occasionally needed to diagnose a spawn — and only ever in this
/// scrubbed form. Routine activity must not be logged with it.
pub fn redacted_env_field(env: &BTreeMap<String, String>) -> String {
    redact_env_map(env)
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests;
