//! `logging` domain — the tracing subscriber that writes koshi's log file.
//!
//! Every `tracing::info!` / `warn!` / `error!` call anywhere in the workspace
//! routes to the one process-wide subscriber [`logging::initialize_tracing`] installs. That
//! subscriber is the single place four questions are answered, all from the
//! `logging` section of `koshi.kdl` — nothing is read from the environment:
//!
//! - **Should this line be written?** [`logging::LoggingParameters::is_enabled`] — disabled
//!   installs no subscriber at all, so no line is written and no file or
//!   `logs/` directory is ever created.
//! - **Where does it go?** The file its [`logging::LogFileOwner`] names, in
//!   `logs/` under the user's state directory ([`logging::resolve_log_directory`]):
//!   `koshi-log-<id>.log` for one session, `koshi-log-router.log` for the
//!   router. The file is created on the *first* line written and re-created if
//!   it is removed while koshi runs; on Unix it is created as `0600` inside a
//!   `0700` directory. Several processes write one session's file — the
//!   session server, each client attached to it, and on Windows the process
//!   holding its panes. Every router under one state directory writes the one
//!   router file; two routers run at once only with two runtime directories.
//!   Every line is one open-append-close, so the lines of two processes
//!   interleave whole.
//! - **How large does it get?** A line that takes a file over
//!   [`logging::LoggingParameters::max_log_file_size_bytes`] moves the file to
//!   `<name>.1`, which replaces the `.1` before it, and the next line starts a
//!   new file. `max-log-file-size-in-mb 4`, the default, gives 4 MiB, and each
//!   name holds about twice the size at most. A session server start deletes
//!   the files of every session but its own and the
//!   [`logging::MAX_OTHER_SESSION_LOG_COUNT`] (20) sessions written last
//!   ([`logging::delete_stale_session_log_files`]).
//! - **What passes the bar?** [`logging::LoggingParameters::log_level`] — the lowest severity
//!   that gets written; a line below it is dropped before it reaches the file.
//!
//! # Logging policy
//!
//! Logs record **errors** and **domain events** — nothing else. They are a trail
//! of *what happened and what triggered it*, not a narration of *what the code
//! was doing*. Each line should carry only the minimum needed to correlate it
//! back to its cause: the correlation IDs it has — `session_id`, `client_id`,
//! `tab_id`, `pane_id`, `command_id`, `subscriber_id` — plus an
//! event or error kind. No payloads, no command arguments, no environment
//! values, no terminal/PTY output, no per-frame or per-keystroke activity.
//! Anything high-frequency or content-like belongs in the recent-events buffer
//! (`koshi debug events`), not the log file.
//!
//! # What each level means
//!
//! The three levels answer one question: *did koshi know what to do about it?*
//!
//! - **`info`** — it worked. A thing koshi does finished: the config applied,
//!   a pane opened, a session started.
//! - **`warn`** — it failed, koshi expected that it might, and koshi had an
//!   answer ready. It kept running on the fallback. A profile that will not
//!   parse starts one plain shell instead; a `keybinding.kdl` with a conflict
//!   leaves the built-in keys in place.
//! - **`error`** — it failed in a way koshi did not anticipate, so there is no
//!   fallback to take. koshi or the client is going down. Entering raw mode
//!   fails and there is no way to draw anything at all.
//!
//! A runtime [`koshi_core::event::Event`] is never an `error`. Errors are
//! written at the startup and teardown steps that have no fallback. Events are
//! classified in [`logging::event_log`].
//!
//! Logs never go to stdout.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use thiserror::Error;
use tracing::Level;
use tracing_subscriber::fmt;
use tracing_subscriber::fmt::MakeWriter;

use koshi_core::ids::{parse_prefixed_uuid, SessionId};
use koshi_core::log::{LogFormat, LogLevel};

/// Writing a log line for a committed runtime event.
pub mod event_log;
/// Keeping the last committed runtime events for `koshi debug events`.
pub mod recent_events;

/// How many other sessions' log files a session server start keeps: 20, the
/// sessions whose files were written last.
pub const MAX_OTHER_SESSION_LOG_COUNT: usize = 20;

/// The file in the log directory that a writer locks while it moves a log file
/// to `<name>.1`: `rotate.lock`.
const ROTATE_LOCK_FILE_NAME: &str = "rotate.lock";

/// Everything the subscriber needs, resolved from the `logging` config section.
#[derive(Debug, Clone)]
pub struct LoggingParameters {
    /// Whether to install a subscriber and write a file at all.
    pub is_enabled: bool,
    /// The lowest severity that gets written.
    pub log_level: LogLevel,
    /// How each written line is rendered.
    pub log_format: LogFormat,
    /// The process that writes the log file, which names that file.
    pub log_file_owner: LogFileOwner,
    /// The size in bytes a log file may reach. A line that takes the file over
    /// it moves the file to `<name>.1`.
    pub max_log_file_size_bytes: u64,
}

/// The process a log file belongs to. [`resolve_log_file_path`] gives the
/// file it names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFileOwner {
    /// A process serving one session, or attached to it: the session server,
    /// each client attached to it, and on Windows the process holding its
    /// panes. Names `koshi-log-<uuid>.log`.
    Session {
        /// The session the process serves or is attached to.
        session_id: SessionId,
    },
    /// The router. Names `koshi-log-router.log`.
    Router,
}

/// The directory every log file goes in: `logs/` under the user's state
/// directory (resolved by [`koshi_paths::resolve_state_directory`]) —
/// `~/.local/state/koshi/logs` on Linux, `~/Library/Application
/// Support/koshi/logs` on macOS, `%LOCALAPPDATA%\koshi\data\logs` on Windows.
/// `None` when no home directory can be found at all.
#[must_use]
pub fn resolve_log_directory() -> Option<PathBuf> {
    koshi_paths::resolve_state_directory()
        .map(|state_directory_path| state_directory_path.join("logs"))
}

/// The log file `log_file_owner` writes, in [`resolve_log_directory`]:
/// `koshi-log-<uuid>.log` for [`LogFileOwner::Session`] and
/// `koshi-log-router.log` for [`LogFileOwner::Router`]. If no home directory
/// can be found at all, the file lands in the current directory as a last
/// resort.
///
/// Example: session `…446655440000` resolves on Linux to
/// `~/.local/state/koshi/logs/koshi-log-…446655440000.log`, and the router to
/// `~/.local/state/koshi/logs/koshi-log-router.log`.
#[must_use]
pub fn resolve_log_file_path(log_file_owner: LogFileOwner) -> PathBuf {
    let log_file_name = format_log_file_name(log_file_owner);
    match resolve_log_directory() {
        Some(log_directory) => log_directory.join(log_file_name),
        None => PathBuf::from(log_file_name),
    }
}

/// The name of the file `log_file_owner` writes: `koshi-log-<uuid>.log` for
/// [`LogFileOwner::Session`], with the UUID in lowercase hyphenated form, and
/// `koshi-log-router.log` for [`LogFileOwner::Router`].
fn format_log_file_name(log_file_owner: LogFileOwner) -> String {
    match log_file_owner {
        LogFileOwner::Session { session_id } => format!("koshi-log-{}.log", session_id.get_uuid()),
        LogFileOwner::Router => "koshi-log-router.log".to_string(),
    }
}

/// The session whose log file `log_file_name` names: `koshi-log-<uuid>.log`
/// or `koshi-log-<uuid>.log.1`, exactly as [`format_log_file_name`] gives it.
/// `None` for every other name: the router's files, `rotate.lock`, a UUID in
/// another form such as uppercase, and `koshi-log-<uuid>.log.2`.
fn find_log_file_session_id(log_file_name: &str) -> Option<SessionId> {
    let current_log_file_name = log_file_name.strip_suffix(".1").unwrap_or(log_file_name);
    let session_uuid_text = current_log_file_name
        .strip_prefix("koshi-log-")?
        .strip_suffix(".log")?;
    let session_id = SessionId::from_uuid(parse_prefixed_uuid(session_uuid_text, "session").ok()?);
    if format_log_file_name(LogFileOwner::Session { session_id }) != current_log_file_name {
        return None;
    }
    Some(session_id)
}

/// A log file, or the log directory, that [`delete_stale_session_log_files`]
/// could not read or delete.
#[derive(Debug, Error)]
#[error("{}: {io_error}", log_path.display())]
pub struct SessionLogDeleteError {
    /// The file or directory the failed read or delete named.
    pub log_path: PathBuf,
    /// The error the read or the delete returned.
    pub io_error: io::Error,
}

/// The log files of one session other than the caller's own, found in the log
/// directory.
struct SessionLogFiles {
    /// The session the files belong to.
    session_id: SessionId,
    /// The newest modification time among the files.
    last_written_at: SystemTime,
    /// `koshi-log-<uuid>.log`, `koshi-log-<uuid>.log.1`, or both.
    log_file_paths: Vec<PathBuf>,
}

/// Delete the log files of every session in [`resolve_log_directory`] but
/// `own_session_id` and the [`MAX_OTHER_SESSION_LOG_COUNT`] sessions whose
/// files were written last. A session's files are `koshi-log-<uuid>.log` and
/// `koshi-log-<uuid>.log.1`; the newest modification time of the two is the
/// session's. Every other name in the directory, the router's files and
/// `rotate.lock` among them, is left alone, and so is every entry that is not
/// a regular file. Two sessions written at the same time keep the newer
/// session id first.
///
/// Example: a directory holding the files of 23 other sessions keeps the 20
/// written last and deletes the files of the 3 written first.
///
/// No home directory and no log directory delete nothing. A file that is
/// already gone counts as deleted. A session with a file whose modification
/// time cannot be read keeps every file it has.
///
/// # Errors
/// The first [`SessionLogDeleteError`]: the log directory that could not be
/// read, or a file whose modification time could not be read or that could
/// not be deleted. Every other file is still read and deleted.
pub fn delete_stale_session_log_files(
    own_session_id: SessionId,
) -> Result<(), SessionLogDeleteError> {
    match resolve_log_directory() {
        Some(log_directory) => {
            delete_stale_session_log_files_in_directory(&log_directory, own_session_id)
        }
        None => Ok(()),
    }
}

/// [`delete_stale_session_log_files`] in `log_directory`, as given.
fn delete_stale_session_log_files_in_directory(
    log_directory: &Path,
    own_session_id: SessionId,
) -> Result<(), SessionLogDeleteError> {
    let directory_entries = match std::fs::read_dir(log_directory) {
        Ok(directory_entries) => directory_entries,
        Err(read_error) if read_error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(read_error) => {
            return Err(SessionLogDeleteError {
                log_path: log_directory.to_path_buf(),
                io_error: read_error,
            })
        }
    };
    let mut first_delete_error: Option<SessionLogDeleteError> = None;
    let mut session_log_files_by_session_id: BTreeMap<SessionId, SessionLogFiles> = BTreeMap::new();
    let mut undated_session_ids: BTreeSet<SessionId> = BTreeSet::new();
    for directory_entry in directory_entries {
        let directory_entry = match directory_entry {
            Ok(directory_entry) => directory_entry,
            Err(entry_read_error) => {
                first_delete_error.get_or_insert(SessionLogDeleteError {
                    log_path: log_directory.to_path_buf(),
                    io_error: entry_read_error,
                });
                continue;
            }
        };
        let Some(session_id) = directory_entry
            .file_name()
            .to_str()
            .and_then(find_log_file_session_id)
        else {
            continue;
        };
        if session_id == own_session_id {
            continue;
        }
        let log_file_path = directory_entry.path();
        let entry_metadata = match directory_entry.metadata() {
            Ok(entry_metadata) => entry_metadata,
            Err(metadata_error) if metadata_error.kind() == io::ErrorKind::NotFound => continue,
            Err(metadata_error) => {
                undated_session_ids.insert(session_id);
                first_delete_error.get_or_insert(SessionLogDeleteError {
                    log_path: log_file_path,
                    io_error: metadata_error,
                });
                continue;
            }
        };
        if !entry_metadata.is_file() {
            continue;
        }
        let written_at = match entry_metadata.modified() {
            Ok(written_at) => written_at,
            Err(modified_time_error) => {
                undated_session_ids.insert(session_id);
                first_delete_error.get_or_insert(SessionLogDeleteError {
                    log_path: log_file_path,
                    io_error: modified_time_error,
                });
                continue;
            }
        };
        let session_log_files =
            session_log_files_by_session_id
                .entry(session_id)
                .or_insert(SessionLogFiles {
                    session_id,
                    last_written_at: written_at,
                    log_file_paths: Vec::new(),
                });
        session_log_files.last_written_at = session_log_files.last_written_at.max(written_at);
        session_log_files.log_file_paths.push(log_file_path);
    }

    let mut other_session_log_files: Vec<SessionLogFiles> = session_log_files_by_session_id
        .into_values()
        .filter(|session_log_files| !undated_session_ids.contains(&session_log_files.session_id))
        .collect();
    other_session_log_files.sort_by_key(|session_log_files| {
        Reverse((
            session_log_files.last_written_at,
            session_log_files.session_id,
        ))
    });
    for stale_session_log_files in other_session_log_files
        .iter()
        .skip(MAX_OTHER_SESSION_LOG_COUNT)
    {
        for log_file_path in &stale_session_log_files.log_file_paths {
            match std::fs::remove_file(log_file_path) {
                Ok(()) => {}
                Err(delete_error) if delete_error.kind() == io::ErrorKind::NotFound => {}
                Err(delete_error) => {
                    first_delete_error.get_or_insert(SessionLogDeleteError {
                        log_path: log_file_path.clone(),
                        io_error: delete_error,
                    });
                }
            }
        }
    }
    match first_delete_error {
        Some(delete_error) => Err(delete_error),
        None => Ok(()),
    }
}

/// An error from [`initialize_tracing`].
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
/// a subscriber that writes the file [`resolve_log_file_path`] names for the
/// [`LoggingParameters::log_file_owner`], lazily on the first line.
///
/// Returns [`TracingError::AlreadyInitialized`] if a subscriber is already
/// installed.
pub fn initialize_tracing(logging_parameters: LoggingParameters) -> Result<(), TracingError> {
    if !logging_parameters.is_enabled {
        return Ok(());
    }
    initialize_tracing_at_path(
        &resolve_log_file_path(logging_parameters.log_file_owner),
        logging_parameters.log_level,
        logging_parameters.log_format,
        logging_parameters.max_log_file_size_bytes,
    )
}

/// Install a subscriber writing to `log_file_path`, which moves to
/// `<name>.1` over `max_log_file_size_bytes`. [`initialize_tracing`] resolves
/// the path from the log file owner; this takes the path as given.
fn initialize_tracing_at_path(
    log_file_path: &Path,
    log_level: LogLevel,
    log_format: LogFormat,
    max_log_file_size_bytes: u64,
) -> Result<(), TracingError> {
    let log_writer = LogFileMaker {
        log_file_path: log_file_path.to_path_buf(),
        max_log_file_size_bytes,
    };
    // `with_ansi(false)` keeps the file plain text. The format method
    // (`pretty`/`json`) is the only thing that differs per arm.
    let subscriber_builder = fmt()
        .with_max_level(resolve_maximum_tracing_level(log_level))
        .with_ansi(false)
        .with_writer(log_writer);
    let initialization_result = match log_format {
        LogFormat::Pretty => subscriber_builder.pretty().try_init(),
        LogFormat::Json => subscriber_builder.json().try_init(),
    };
    initialization_result.map_err(|_| TracingError::AlreadyInitialized)
}

/// The most verbose severity that still gets written for a configured level:
/// `warning` admits warnings and errors, `error` admits only errors.
fn resolve_maximum_tracing_level(log_level: LogLevel) -> Level {
    match log_level {
        LogLevel::Info => Level::INFO,
        LogLevel::Warning => Level::WARN,
        LogLevel::Error => Level::ERROR,
    }
}

/// A [`MakeWriter`] that appends formatted events to one log file.
/// The subscriber creates one writer for each event. Each write creates the
/// file when it is missing and attempts to create its parent when the open
/// fails, so a removed file or directory comes back on the next event. For
/// `logs/koshi-log-abc.log`, a missing `logs` directory is recreated after the
/// open fails.
struct LogFileMaker {
    log_file_path: PathBuf,
    max_log_file_size_bytes: u64,
}

impl<'a> MakeWriter<'a> for LogFileMaker {
    type Writer = LogFileWriter;

    fn make_writer(&'a self) -> Self::Writer {
        LogFileWriter {
            log_file_path: self.log_file_path.clone(),
            max_log_file_size_bytes: self.max_log_file_size_bytes,
        }
    }
}

/// The `io::Write` half of [`LogFileMaker`]: opens the file in
/// create-and-append mode for one event's bytes, writes them, and closes it.
/// Every line is written out before the next event.
///
/// When the file it wrote to is over `max_log_file_size_bytes` after the
/// write, it moves the file to `<name>.1`, as [`rotate_log_file`] states. A
/// line that a handle opened before the move writes lands in `<name>.1`.
///
/// On Unix a file this writer creates is mode `0600` and a directory it
/// creates is mode `0700`, so no other local user reads the log. A file or
/// directory that already exists keeps the mode it has.
struct LogFileWriter {
    log_file_path: PathBuf,
    max_log_file_size_bytes: u64,
}

impl io::Write for LogFileWriter {
    fn write(&mut self, log_bytes: &[u8]) -> io::Result<usize> {
        let log_file_byte_count = match append_log_bytes_to_file(&self.log_file_path, log_bytes) {
            Ok(log_file_byte_count) => log_file_byte_count,
            Err(_) => {
                // A `logs/` directory removed while koshi runs makes the open
                // fail; creating the parent and appending again brings the
                // file back. An error from either step is returned.
                if let Some(parent_directory_path) = self.log_file_path.parent() {
                    create_private_directory_with_parents(parent_directory_path)?;
                }
                append_log_bytes_to_file(&self.log_file_path, log_bytes)?
            }
        };
        // A move that fails leaves the file as it is; the next line that
        // finds the file over the size moves it.
        if log_file_byte_count > self.max_log_file_size_bytes {
            let _ = rotate_log_file(&self.log_file_path, self.max_log_file_size_bytes);
        }
        Ok(log_bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Append `log_bytes` to the file at `log_file_path` in create-and-append mode, then close
/// it. On Unix a file this creates is mode `0600`.
///
/// Returns the length of the file the bytes went to, read from the open
/// handle after the write. A length that cannot be read is `0`.
fn append_log_bytes_to_file(log_file_path: &Path, log_bytes: &[u8]) -> io::Result<u64> {
    use io::Write as _;

    let mut log_file = build_private_file_open_options()
        .append(true)
        .open(log_file_path)?;
    log_file.write_all(log_bytes)?;
    Ok(log_file
        .metadata()
        .map_or(0, |log_file_metadata| log_file_metadata.len()))
}

/// Move the log file at `log_file_path` to `<name>.1` when it is over
/// `max_log_file_size_bytes`, replacing the `<name>.1` before it.
///
/// 1. Opens `rotate.lock` beside the file, creating it as a log file is
///    created, and takes its exclusive lock without waiting. A lock that
///    another writer holds ends the call, and the next line over the size
///    tries again.
/// 2. Reads the size of the file at `log_file_path` again. A file that is
///    gone or at most `max_log_file_size_bytes`, such as the new file after
///    another writer's move, ends the call.
/// 3. Renames the file to `<name>.1`, then lets the lock go.
///
/// Example: with `max_log_file_size_bytes` 4194304,
/// `logs/koshi-log-router.log` at 4 MiB plus one line becomes
/// `logs/koshi-log-router.log.1`, and the next line creates a new
/// `logs/koshi-log-router.log`.
///
/// # Errors
/// The error of opening or locking `rotate.lock`, of reading the file's size,
/// or of the rename. The file stays where it is.
fn rotate_log_file(log_file_path: &Path, max_log_file_size_bytes: u64) -> io::Result<()> {
    let rotate_lock_file = build_private_file_open_options()
        .write(true)
        .truncate(false)
        .open(log_file_path.with_file_name(ROTATE_LOCK_FILE_NAME))?;
    match rotate_lock_file.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return Ok(()),
        Err(TryLockError::Error(lock_error)) => return Err(lock_error),
    }
    let rename_result = match std::fs::metadata(log_file_path) {
        Ok(log_file_metadata) if log_file_metadata.len() > max_log_file_size_bytes => {
            let mut previous_log_file_path = log_file_path.as_os_str().to_owned();
            previous_log_file_path.push(".1");
            std::fs::rename(log_file_path, previous_log_file_path)
        }
        Ok(_) => Ok(()),
        Err(metadata_error) if metadata_error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(metadata_error) => Err(metadata_error),
    };
    let _ = rotate_lock_file.unlock();
    rename_result
}

/// [`OpenOptions`] that create the file when it is missing. On Unix a file
/// they create is mode `0600`. The caller sets the access mode.
fn build_private_file_open_options() -> OpenOptions {
    let mut open_options = OpenOptions::new();
    open_options.create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        open_options.mode(0o600);
    }
    open_options
}

/// Create `directory_path` and any missing parent. On Unix every directory this creates
/// is mode `0700`. A directory already at `directory_path` is success; a regular file
/// there is [`io::ErrorKind::AlreadyExists`].
fn create_private_directory_with_parents(directory_path: &Path) -> io::Result<()> {
    let mut directory_builder = std::fs::DirBuilder::new();
    directory_builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        directory_builder.mode(0o700);
    }
    directory_builder.create(directory_path)
}

/// Captured log bytes. [`with_test_writer`] installs the subscriber on the
/// calling thread and returns these bytes for assertions. For an emitted
/// `tracing::info!("ready")`, [`contents`](Self::contents) includes its line.
#[derive(Clone, Default)]
pub struct CapturedLogs {
    captured_log_bytes: Arc<Mutex<Vec<u8>>>,
}

impl CapturedLogs {
    /// All captured output as a single string. A poisoned lock is recovered;
    /// the bytes written before the poisoning are returned.
    pub fn contents(&self) -> String {
        let captured_log_bytes = self
            .captured_log_bytes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        String::from_utf8_lossy(&captured_log_bytes).into_owned()
    }

    /// The captured output split into lines (one JSON record per line).
    pub fn lines(&self) -> Vec<String> {
        self.contents().lines().map(str::to_owned).collect()
    }
}

/// The `io::Write` end of [`CapturedLogs`], handed to the fmt layer.
pub struct CapturedWriter {
    captured_log_bytes: Arc<Mutex<Vec<u8>>>,
}

impl io::Write for CapturedWriter {
    fn write(&mut self, log_bytes: &[u8]) -> io::Result<usize> {
        self.captured_log_bytes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .extend_from_slice(log_bytes);
        Ok(log_bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for CapturedLogs {
    type Writer = CapturedWriter;

    fn make_writer(&'a self) -> Self::Writer {
        CapturedWriter {
            captured_log_bytes: Arc::clone(&self.captured_log_bytes),
        }
    }
}

/// Install a JSON subscriber scoped to the current thread and capture its output.
///
/// The subscriber is the calling thread's default; other threads and the
/// global subscriber are untouched. Drop the guard to restore the previous
/// subscriber; read the [`CapturedLogs`] to assert on log output.
///
/// The first call registers a process-wide anchor dispatcher
/// (`register_interest_anchor`) that keeps captures visible to call sites
/// first fired on threads with no subscriber.
pub fn with_test_writer() -> (tracing::subscriber::DefaultGuard, CapturedLogs) {
    capture_logs_at_level(Level::TRACE)
}

/// Install a JSON subscriber capped at `maximum_log_level` as the calling thread's default
/// and capture its output. Registers the interest anchor first.
fn capture_logs_at_level(
    maximum_log_level: Level,
) -> (tracing::subscriber::DefaultGuard, CapturedLogs) {
    register_interest_anchor();
    let captured_logs = CapturedLogs::default();
    let subscriber = fmt()
        .with_max_level(maximum_log_level)
        .json()
        .with_writer(captured_logs.clone())
        .finish();
    let subscriber_guard = tracing::subscriber::set_default(subscriber);
    (subscriber_guard, captured_logs)
}

/// Register one TRACE-level dispatcher in tracing's dispatcher registry for
/// the life of the process, without making it a thread or global default.
///
/// With it registered, an event reaches a [`with_test_writer`] capture even
/// when its call site first fired on a thread with no subscriber. The anchor
/// never formats an event; its writer is unused.
fn register_interest_anchor() {
    static INTEREST_ANCHOR: std::sync::Once = std::sync::Once::new();
    INTEREST_ANCHOR.call_once(|| {
        let anchor_dispatch = fmt()
            .with_max_level(Level::TRACE)
            .with_writer(io::sink as fn() -> io::Sink)
            .finish();
        std::mem::forget(tracing::Dispatch::new(anchor_dispatch));
    });
}

#[cfg(test)]
mod tests;
