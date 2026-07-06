//! `logging` domain — structured logging bootstrap.
//!
//! [`logging::init_tracing`] installs a process-wide subscriber that emits structured
//! logs, formatted as JSON or human-readable text per [`logging::TracingOptions`]. Logs
//! carry a fixed set of [canonical fields](self#canonical-fields) so a session
//! can be followed across panes, commands, and plugins.
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
//! This keeps the file small over a long session and free of user data.
//!
//! Logs never go to stdout: that is Koshi's render surface, and writing to it
//! would corrupt the terminal UI. The default [`logging::LogDestination`] is a file under
//! the user's state directory (the sink behind `koshi debug tail-log`); [stderr]
//! is offered for non-UI contexts such as early startup or a foreground daemon.
//!
//! [stderr]: logging::LogDestination::Stderr
//!
//! Redaction is not optional: anything derived from the environment must pass
//! through [`logging::redacted_env_field`] before it becomes a log value, so a secret
//! such as `KOSHI_CONTEXT_TOKEN` can never reach the output even if it is handed
//! to the logger by mistake. The scrubbing itself lives in [`koshi_core::redact`];
//! this module only routes env maps through it on the way to a log line.
//!
//! Environment variables read by [`logging::TracingOptions::from_env`]:
//! - `KOSHI_LOG_FORMAT` — `json` or `pretty` (default: `pretty`).
//! - `KOSHI_LOG` — tracing filter directive, e.g. `info` or `koshi=debug`
//!   (default: `info`).

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use directories::ProjectDirs;
use thiserror::Error;
use tracing_appender::non_blocking::{NonBlocking, WorkerGuard};
use tracing_appender::rolling::{Builder, Rotation};
use tracing_subscriber::fmt;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::EnvFilter;

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

/// How log lines are rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    /// One JSON object per line, for machine ingestion.
    Json,
    /// Human-readable multi-line records, for a developer at a terminal.
    Pretty,
}

impl LogFormat {
    /// Read the format from `KOSHI_LOG_FORMAT` (`json` or `pretty`). Anything else,
    /// including an unset variable, falls back to [`LogFormat::Pretty`].
    pub fn from_env() -> Self {
        LogFormat::parse(std::env::var("KOSHI_LOG_FORMAT").ok().as_deref())
    }

    /// The pure mapping behind [`Self::from_env`]: `Some("json")` is JSON, anything else
    /// (including `None`) is pretty. Kept separate so it can be tested without
    /// touching the process-global environment.
    pub fn parse(value: Option<&str>) -> Self {
        match value {
            Some("json") => LogFormat::Json,
            _ => LogFormat::Pretty,
        }
    }
}

/// Where log lines are written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogDestination {
    /// A file. The default for a running session, since stdout is the render
    /// surface and even stderr smears a full-screen terminal UI.
    File(PathBuf),
    /// The standard error stream, for contexts with no terminal UI (early
    /// startup, a foreground daemon, one-shot commands).
    Stderr,
    /// No logging at all. `init_tracing` installs nothing and touches no disk:
    /// no directory, no file. Distinct from a filter of `off`, which still opens
    /// a (then-empty) log file.
    Disabled,
}

impl LogDestination {
    /// The default destination: a `koshi.log` file under the user's state
    /// directory (see [`default_log_path`]). A file destination rotates daily and
    /// keeps a bounded number of days (see [`TracingOptions::max_log_files`]).
    pub fn default_file() -> Self {
        LogDestination::File(default_log_path())
    }
}

/// How many rotated log files to keep before the oldest is deleted.
pub const DEFAULT_MAX_LOG_FILES: usize = 7;

/// The default log file is `koshi.log` under the OS state directory, resolved by
/// [`directories::ProjectDirs`]: `$XDG_STATE_HOME/koshi` (or `~/.local/state/koshi`)
/// on Linux, `~/Library/Application Support/koshi` on macOS, and
/// `%LOCALAPPDATA%\koshi` on Windows. macOS and Windows have no distinct "state"
/// location, so `state_dir()` is `None` there and we fall back to the per-user
/// data directory. If no home directory can be found at all, the file lands in
/// the current directory as a last resort.
pub fn default_log_path() -> PathBuf {
    match ProjectDirs::from("", "", "koshi") {
        Some(dirs) => dirs
            .state_dir()
            .unwrap_or_else(|| dirs.data_local_dir())
            .join("koshi.log"),
        None => PathBuf::from("koshi.log"),
    }
}

/// Knobs for [`init_tracing`].
#[derive(Debug, Clone)]
pub struct TracingOptions {
    /// How log lines are rendered.
    pub format: LogFormat,
    /// A `tracing_subscriber` env-filter directive (e.g. `info`, `koshi=debug`).
    pub filter: String,
    /// Where log lines are written.
    pub destination: LogDestination,
    /// For a file destination, how many daily-rotated files to retain before the
    /// oldest is deleted. Ignored for [`LogDestination::Stderr`].
    pub max_log_files: usize,
}

impl TracingOptions {
    /// Build options from the environment: [`LogFormat::from_env`] for the format,
    /// `KOSHI_LOG` (defaulting to `info`) for the filter, the default log file for
    /// the destination, and [`DEFAULT_MAX_LOG_FILES`] for retention.
    pub fn from_env() -> Self {
        TracingOptions {
            format: LogFormat::from_env(),
            filter: std::env::var("KOSHI_LOG").unwrap_or_else(|_| "info".to_string()),
            destination: LogDestination::default_file(),
            max_log_files: DEFAULT_MAX_LOG_FILES,
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
    /// The log file or its directory could not be opened.
    #[error("could not open log sink: {0}")]
    Sink(String),
    /// A global subscriber was already installed for this process.
    #[error("tracing is already initialized for this process")]
    AlreadyInitialized,
}

/// Holds resources tied to the active subscriber. Keep it alive for as long as
/// logging is needed.
///
/// For a file destination it owns the non-blocking writer's worker guard, which
/// flushes buffered log lines to disk on drop; dropping it early can therefore
/// lose tail-end logs. For [`LogDestination::Stderr`] there is nothing to flush
/// and dropping it is a no-op.
pub struct TracingGuard {
    _worker: Option<WorkerGuard>,
}

/// Install the process-wide tracing subscriber.
///
/// Returns [`TracingError::AlreadyInitialized`] if called more than once, since a
/// process has a single global subscriber.
pub fn init_tracing(opts: TracingOptions) -> Result<TracingGuard, TracingError> {
    // Disabled means do nothing: with no global subscriber installed, every
    // tracing event is dropped, and no sink is opened. Return before parsing the
    // filter or touching the filesystem.
    if opts.destination == LogDestination::Disabled {
        return Ok(TracingGuard { _worker: None });
    }

    let filter =
        EnvFilter::try_new(&opts.filter).map_err(|err| TracingError::Filter(err.to_string()))?;
    let max_log_files = opts.max_log_files;

    // Each (format, destination) pair builds a differently typed subscriber, so
    // the arms install their own and report back whether init succeeded plus the
    // worker guard to keep alive.
    let (result, worker) = match (opts.format, opts.destination) {
        (LogFormat::Json, LogDestination::Stderr) => (
            fmt()
                .with_env_filter(filter)
                .json()
                .with_writer(io::stderr)
                .try_init(),
            None,
        ),
        (LogFormat::Pretty, LogDestination::Stderr) => (
            fmt()
                .with_env_filter(filter)
                .pretty()
                .with_writer(io::stderr)
                .try_init(),
            None,
        ),
        (LogFormat::Json, LogDestination::File(path)) => {
            let (writer, worker) = file_writer(&path, max_log_files)?;
            (
                fmt()
                    .with_env_filter(filter)
                    .json()
                    .with_writer(writer)
                    .try_init(),
                Some(worker),
            )
        }
        (LogFormat::Pretty, LogDestination::File(path)) => {
            let (writer, worker) = file_writer(&path, max_log_files)?;
            (
                fmt()
                    .with_env_filter(filter)
                    .pretty()
                    .with_writer(writer)
                    .try_init(),
                Some(worker),
            )
        }
        // Handled by the early return above.
        (_, LogDestination::Disabled) => unreachable!("disabled destination returns early"),
    };
    result.map_err(|_| TracingError::AlreadyInitialized)?;

    Ok(TracingGuard { _worker: worker })
}

/// Open `path` as a non-blocking, daily-rotated log sink, creating its parent
/// directory. `path`'s file name is the rotation prefix, so files land as
/// `<name>.YYYY-MM-DD`; at most `max_log_files` are kept before the oldest is
/// deleted. Returns the writer plus the worker guard that flushes it on drop.
fn file_writer(
    path: &Path,
    max_log_files: usize,
) -> Result<(NonBlocking, WorkerGuard), TracingError> {
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    let prefix = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            TracingError::Sink(format!("log path has no file name: {}", path.display()))
        })?;
    std::fs::create_dir_all(directory).map_err(|err| TracingError::Sink(err.to_string()))?;
    // Clamped to one: retention of zero would prune every file, including
    // the one being written — logs would silently vanish.
    let appender = Builder::new()
        .rotation(Rotation::DAILY)
        .filename_prefix(prefix)
        .max_log_files(max_log_files.max(1))
        .build(directory)
        .map_err(|err| TracingError::Sink(err.to_string()))?;
    Ok(tracing_appender::non_blocking(appender))
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
