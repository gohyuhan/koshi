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
//!   koshi runs. Two processes write one session's file — the session server
//!   and the client attached to it — and every line is one open-append-close,
//!   so the two processes' lines interleave whole.
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
//! belongs in the recent-events buffer (`koshi debug events`), not the log file.
//!
//! # What each level means
//!
//! The three levels answer one question: *did koshi know what to do about it?*
//!
//! - **`info`** — it worked. A thing koshi does finished: the config applied,
//!   a pane opened, a session started, a plugin loaded.
//! - **`warn`** — it failed, koshi expected that it might, and koshi had an
//!   answer ready. It kept running on the fallback. A profile that will not
//!   parse starts one plain shell instead; a `keybinding.kdl` with a conflict
//!   leaves the built-in keys in place.
//! - **`error`** — it failed in a way koshi did not anticipate, so there is no
//!   fallback to take. koshi or the client is going down. Entering raw mode
//!   fails and there is no way to draw anything at all.
//!
//! So a runtime [`koshi_core::event::Event`] is never an `error`: every variant
//! of it is a fact koshi modelled in advance. Errors are written at the startup
//! and teardown steps that have nowhere to fall back to. Events are classified
//! in [`logging::event_log`].
//!
//! Logs never go to stdout, which is Koshi's render surface.
//!
//! Anything derived from the environment must pass through
//! [`logging::redacted_env_field`] before it becomes a log value, so a secret
//! such as `KOSHI_CONTEXT_TOKEN` cannot reach the output. The scrubbing itself
//! lives in [`koshi_core::redact`]; this module only routes env maps through it
//! on the way to a log line.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use thiserror::Error;
use tracing::Level;
use tracing_subscriber::fmt;
use tracing_subscriber::fmt::MakeWriter;

use koshi_core::ids::SessionId;
use koshi_core::log::{LogFormat, LogLevel};
use koshi_core::redact::redact_env_map;

/// Writing a log line for a committed runtime event.
pub mod event_log;
/// Keeping the last committed runtime events for `koshi debug events`.
pub mod recent_events;

/// The canonical field names every cross-cutting log line should carry. They are
/// correlation IDs — the join keys for tracing one event back to its cause across
/// panes, commands, and plugins — not descriptions of state or activity.
pub const CANONICAL_FIELDS: [&str; 7] = [
    "session_id",
    "client_id",
    "tab_id",
    "pane_id",
    "command_id",
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

/// The directory every log file goes in: `logs/` under the user's state
/// directory (resolved by [`koshi_paths::state_dir`]) —
/// `~/.local/state/koshi/logs` on Linux, `~/Library/Application
/// Support/koshi/logs` on macOS, `%LOCALAPPDATA%\koshi\data\logs` on Windows.
/// `None` when no home directory can be found at all.
#[must_use]
pub fn log_dir() -> Option<PathBuf> {
    koshi_paths::state_dir().map(|dir| dir.join("logs"))
}

/// The log file for `session_id`: `koshi-log-<uuid>.log` in [`log_dir`]. If no
/// home directory can be found at all, the file lands in the relative `logs/`
/// directory as a last resort.
///
/// Example: session `…446655440000` resolves on Linux to
/// `~/.local/state/koshi/logs/koshi-log-…446655440000.log`.
#[must_use]
pub fn session_log_path(session_id: SessionId) -> PathBuf {
    let name = format!("koshi-log-{}.log", session_id.as_uuid());
    match log_dir() {
        Some(dir) => dir.join(name),
        None => PathBuf::from("logs").join(name),
    }
}

const LOG_FILE_PREFIX: &str = "koshi-log-";
const LOG_FILE_SUFFIX: &str = ".log";
const MAX_TAIL_LINES: usize = 1000;

/// Why [`tail_logs`] could not read the local log files.
#[derive(Debug, Error)]
pub enum LogTailError {
    /// The local log directory could not be listed.
    #[error("cannot read log directory {path}: {source}")]
    Directory { path: PathBuf, source: io::Error },
    /// One matching local log file could not be read.
    #[error("cannot read log file {path}: {source}")]
    File { path: PathBuf, source: io::Error },
}

/// Read the last local log lines from every session file in `dir`.
///
/// `oldest_kept` is the inclusive timestamp cutoff. When it is `None`, the
/// newest 1000 lines from each file are returned. Files are
/// ordered by their names, and each file has a header before its lines.
/// Missing `dir` means that logging has not created a file yet and returns an
/// empty answer.
pub fn tail_logs(dir: &Path, oldest_kept: Option<SystemTime>) -> Result<String, LogTailError> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(String::new()),
        Err(source) => {
            return Err(LogTailError::Directory {
                path: dir.to_path_buf(),
                source,
            });
        }
    };

    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| LogTailError::Directory {
            path: dir.to_path_buf(),
            source,
        })?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with(LOG_FILE_PREFIX) || !name.ends_with(LOG_FILE_SUFFIX) {
            continue;
        }
        if entry
            .file_type()
            .map_err(|source| LogTailError::File {
                path: entry.path(),
                source,
            })?
            .is_file()
        {
            paths.push(entry.path());
        }
    }
    paths.sort();

    let mut rendered = String::new();
    for path in paths {
        let contents = std::fs::read_to_string(&path).map_err(|source| LogTailError::File {
            path: path.clone(),
            source,
        })?;
        let lines = selected_log_lines(&contents, oldest_kept);
        if lines.is_empty() {
            continue;
        }
        rendered.push_str("== ");
        rendered.push_str(
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("log"),
        );
        rendered.push_str(" ==\n");
        for line in lines {
            rendered.push_str(line);
            rendered.push('\n');
        }
    }
    Ok(rendered)
}

/// Select recent log lines and keep the result bounded per file.
fn selected_log_lines(contents: &str, oldest_kept: Option<SystemTime>) -> Vec<&str> {
    let mut lines = contents
        .lines()
        .filter(|line| match oldest_kept {
            None => true,
            Some(oldest) => parse_log_timestamp(line).is_some_and(|at| at >= oldest),
        })
        .collect::<Vec<_>>();
    if lines.len() > MAX_TAIL_LINES {
        lines.drain(..lines.len() - MAX_TAIL_LINES);
    }
    lines
}

/// Read tracing-subscriber 0.3's UTC timestamp from a pretty or JSON line.
fn parse_log_timestamp(line: &str) -> Option<SystemTime> {
    const TIMESTAMP_LENGTH: usize = 27;
    let line = line.trim_start();
    let bytes = line.as_bytes();
    let starts_with_timestamp = bytes.first().is_some_and(|byte| byte.is_ascii_digit())
        && bytes.get(4) == Some(&b'-')
        && bytes.get(7) == Some(&b'-')
        && bytes.get(10) == Some(&b'T');
    let start = if starts_with_timestamp {
        0
    } else {
        line.find("\"timestamp\":\"")
            .map(|index| index + "\"timestamp\":\"".len())?
    };
    let timestamp = bytes.get(start..start + TIMESTAMP_LENGTH)?;
    if timestamp[4] != b'-'
        || timestamp[7] != b'-'
        || timestamp[10] != b'T'
        || timestamp[13] != b':'
        || timestamp[16] != b':'
        || timestamp[19] != b'.'
        || timestamp[26] != b'Z'
    {
        return None;
    }
    let year = decimal(&timestamp[0..4])?;
    let month = decimal(&timestamp[5..7])?;
    let day = decimal(&timestamp[8..10])?;
    let hour = decimal(&timestamp[11..13])?;
    let minute = decimal(&timestamp[14..16])?;
    let second = decimal(&timestamp[17..19])?;
    let micros = decimal(&timestamp[20..26])?;
    if month == 0
        || month > 12
        || day == 0
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }
    let days = days_from_civil(year, month, day);
    let seconds = days
        .checked_mul(86_400)?
        .checked_add(i64::from(hour) * 3_600)?
        .checked_add(i64::from(minute) * 60)?
        .checked_add(i64::from(second))?;
    if seconds < 0 {
        return None;
    }
    UNIX_EPOCH
        .checked_add(Duration::from_secs(seconds as u64))?
        .checked_add(Duration::from_micros(u64::from(micros)))
}

/// Read a fixed-width decimal field.
fn decimal(bytes: &[u8]) -> Option<u32> {
    let mut value: u32 = 0;
    for byte in bytes {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value.checked_mul(10)?.checked_add(u32::from(byte - b'0'))?;
    }
    Some(value)
}

/// Return the number of days in one Gregorian month.
fn days_in_month(year: u32, month: u32) -> u32 {
    match month {
        2 if year.is_multiple_of(400) || (year.is_multiple_of(4) && !year.is_multiple_of(100)) => {
            29
        }
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Convert a Gregorian date to days since 1970-01-01.
fn days_from_civil(year: u32, month: u32, day: u32) -> i64 {
    let year = i64::from(year) - i64::from(month <= 2);
    let era = if year >= 0 {
        year / 400
    } else {
        (year - 399) / 400
    };
    let year_of_era = year - era * 400;
    let month = i64::from(month);
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
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

/// Install a subscriber writing to `path`, bypassing the state-directory
/// resolver [`init_tracing`] uses, so a test can point it at a temp directory.
pub fn init_to_path(path: &Path, level: LogLevel, format: LogFormat) -> Result<(), TracingError> {
    let writer = SessionLogMaker {
        path: path.to_path_buf(),
    };
    // `with_ansi(false)` keeps the file plain text. The format method
    // (`pretty`/`json`) is the only thing that differs per arm.
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
/// file, creating the file — and, when the open fails, its `logs/` parent —
/// on each write, so a file or directory removed while koshi runs comes back
/// on the next line.
///
/// Every line is one open-append-close, which is what lets a log file deleted
/// mid-session come back on the next line. On a local disk that costs about
/// 25µs per line. The write runs on the runtime's dispatch thread, so a command
/// committing several events pays it once per event before dispatch returns.
// ponytail: reopen-per-line buys surviving `rm` of the log file for the ~25µs
// above. Hold the handle, reopening when a write fails, if dispatch latency
// needs those microseconds back.
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
        if append_to(&self.path, buf).is_err() {
            // A `logs/` directory removed mid-session makes the open fail;
            // one recreate-and-retry brings the file back. A failure that a
            // recreate cannot cure reports from the retry.
            if let Some(parent) = self.path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            append_to(&self.path, buf)?;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Append `buf` to the file at `path` in create-and-append mode, then close
/// it, so the line is on disk when this returns.
fn append_to(path: &std::path::Path, buf: &[u8]) -> io::Result<()> {
    use io::Write as _;

    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(buf)
}

/// A thread-local capture of log output. Returned by [`with_test_writer`] so a
/// test can assert on what was logged.
#[derive(Clone, Default)]
pub struct CapturedLogs {
    buffer: Arc<Mutex<Vec<u8>>>,
}

impl CapturedLogs {
    /// All captured output as a single string. Recovers a poisoned lock instead
    /// of panicking, so bytes written before a writer thread panicked are still
    /// readable.
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
///
/// The first call registers a process-wide anchor dispatcher
/// (`register_interest_anchor`) that keeps captures visible to call sites
/// first fired on threads with no subscriber.
pub fn with_test_writer() -> (tracing::subscriber::DefaultGuard, CapturedLogs) {
    register_interest_anchor();
    let logs = CapturedLogs::default();
    let subscriber = fmt()
        .with_max_level(Level::TRACE)
        .json()
        .with_writer(logs.clone())
        .finish();
    let guard = tracing::subscriber::set_default(subscriber);
    (guard, logs)
}

/// Register one TRACE-level dispatcher in tracing's dispatcher registry for
/// the life of the process, without making it a thread or global default.
///
/// With it registered, tracing computes every call site's cached interest
/// across the registry, so an event reaches a [`with_test_writer`] capture
/// even when its call site first fires on a thread with no subscriber. The
/// anchor never formats an event; its writer is unused.
fn register_interest_anchor() {
    static ANCHOR: std::sync::Once = std::sync::Once::new();
    ANCHOR.call_once(|| {
        let anchor = fmt()
            .with_max_level(Level::TRACE)
            .with_writer(io::sink as fn() -> io::Sink)
            .finish();
        std::mem::forget(tracing::Dispatch::new(anchor));
    });
}

/// Redact an environment map and render it as a single log-safe field value of
/// space-separated `KEY=value` pairs. Sensitive values (per [`koshi_core::redact`])
/// render as `***`. Use this for any env-derived value before logging it.
///
/// Environment is the one payload the [logging policy](self#logging-policy)
/// admits, and only in this scrubbed form.
pub fn redacted_env_field(env: &BTreeMap<String, String>) -> String {
    redact_env_map(env)
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests;
