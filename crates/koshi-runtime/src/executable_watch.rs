//! Whether the koshi program file a server started from now holds another
//! koshi version.
//!
//! [`ExecutableWatch::check_executable_file`] runs on each connection a server
//! accepts. It reads the metadata of the program file, and only when the file
//! differs from the one it examined last, or a restart that did not happen is
//! due to be tried again, does it run `<path> --version`, on a thread of its
//! own. [`ExecutableWatch::check_executable_file_after_refused_hello`] runs
//! when the server refuses a Hello for its protocol version, and reads the
//! version again unless the last read of the same file printed the running
//! version. Example: a session that runs `0.5.0`, whose program file now prints
//! `koshi 0.6.0`, hands `0.6.0` to its caller, and the caller restarts the
//! session into that file.

use std::io::{BufRead, BufReader};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::sync::{mpsc, Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};

use koshi_ipc::endpoint::ServerProgramFile;

#[cfg(test)]
mod tests;

/// How long `<path> --version` has to print its first line: 2 s.
pub const VERSION_ANSWER_WAIT_DURATION: Duration = Duration::from_secs(2);

/// How long after a restart that did not happen a check reads the version of
/// the same program file again: 30 s.
pub const RESTART_RETRY_INTERVAL_DURATION: Duration = Duration::from_secs(30);

/// How long [`read_first_output_line`] waits before it tries again to start a
/// program file that another process holds open for writing: 10 ms.
const BUSY_PROGRAM_FILE_RETRY_INTERVAL_DURATION: Duration = Duration::from_millis(10);

/// The Windows error `CreateProcessW` gives for a program file another
/// process holds open for writing: `ERROR_SHARING_VIOLATION`.
const WINDOWS_SHARING_VIOLATION_ERROR_CODE: i32 = 32;

/// The program file one server started from, and what the server last read
/// of it.
#[derive(Debug)]
pub struct ExecutableWatch {
    /// The path the server started from.
    executable_path: PathBuf,
    /// The version the server runs, such as `0.5.0-pr.1`.
    running_version: String,
    /// What the server last read of the file.
    pub(crate) watch_state: Mutex<WatchState>,
}

/// What an [`ExecutableWatch`] last read of its program file.
#[derive(Debug)]
pub(crate) struct WatchState {
    /// The identity of the file the last version read ran, or of the file
    /// the watch found when it was made. `None` when neither could be read.
    examined_file_identity: Option<ExecutableFileIdentity>,
    /// `true` while a version read runs.
    is_version_read_running: bool,
    /// `true` when the last version read of the examined file printed the
    /// running version. `false` before any read finishes, after a read that
    /// failed or printed another version, and from the start of a read of a
    /// file with another identity.
    is_examined_file_on_running_version: bool,
    /// The instant from which a check reads the version of the examined file
    /// again, set when a restart into that version did not happen. `None`
    /// while no restart is to be tried again.
    pub(crate) restart_retry_at: Option<Instant>,
}

/// One program file at one path: its modification time and byte length, and
/// on Unix the device and inode that hold it. Replacing the file, or writing
/// into it, changes at least one of them.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ExecutableFileIdentity {
    modified_at: SystemTime,
    byte_length: u64,
    #[cfg(unix)]
    device_id: u64,
    #[cfg(unix)]
    inode_number: u64,
}

impl ExecutableFileIdentity {
    /// The identity of the file at `executable_path`. `None` when its metadata
    /// or its modification time cannot be read.
    fn load_from_path(executable_path: &Path) -> Option<ExecutableFileIdentity> {
        let executable_metadata = std::fs::metadata(executable_path).ok()?;
        Some(ExecutableFileIdentity {
            modified_at: executable_metadata.modified().ok()?,
            byte_length: executable_metadata.len(),
            #[cfg(unix)]
            device_id: executable_metadata.dev(),
            #[cfg(unix)]
            inode_number: executable_metadata.ino(),
        })
    }
}

impl ExecutableWatch {
    /// A watch of the program file at `executable_path`, for a server that
    /// runs `running_version` and started from that file. The file as it is
    /// now counts as examined: [`check_executable_file`](Self::check_executable_file)
    /// reads its version only once the file has changed.
    #[must_use]
    pub fn new(executable_path: PathBuf, running_version: &str) -> ExecutableWatch {
        let examined_file_identity = ExecutableFileIdentity::load_from_path(&executable_path);
        ExecutableWatch {
            executable_path,
            running_version: running_version.to_string(),
            watch_state: Mutex::new(WatchState {
                examined_file_identity,
                is_version_read_running: false,
                is_examined_file_on_running_version: false,
                restart_retry_at: None,
            }),
        }
    }

    /// The path of the program file this watch checks.
    #[must_use]
    pub fn get_executable_path(&self) -> &Path {
        &self.executable_path
    }

    /// The program file a server watched by this watch writes beside its
    /// endpoint file: this process's id, the running version, and the
    /// watched path, with each byte sequence that is not valid UTF-8 in the
    /// path replaced by U+FFFD.
    ///
    /// Example: process `5000`, running `0.6.0`, watching
    /// `/usr/local/bin/koshi`, gives the file
    /// `{"file_format":1,"process_id":5000,"build_version":"0.6.0","program_path":"/usr/local/bin/koshi"}`.
    #[must_use]
    pub fn build_server_program_file(&self) -> ServerProgramFile {
        ServerProgramFile {
            process_id: std::process::id(),
            build_version: self.running_version.clone(),
            program_path: self.executable_path.to_string_lossy().into_owned(),
        }
    }

    /// Record that a restart into the version the examined file holds did not
    /// happen. The first check at or after [`RESTART_RETRY_INTERVAL_DURATION`]
    /// from now reads that file's version again, and a file that changes
    /// before then is read at the next check.
    ///
    /// Example: a session whose restart into `0.6.0` was refused at `T` reads
    /// the file's version again at the first connection from `T + 30 s` on,
    /// and asks to restart once more when the file still holds `0.6.0`.
    pub fn schedule_restart_retry(&self) {
        self.watch_state
            .lock()
            .expect("executable watch")
            .restart_retry_at = Some(Instant::now() + RESTART_RETRY_INTERVAL_DURATION);
    }

    /// Read the identity of the program file, and when it differs from the
    /// file the last version read ran, run `<path> --version` on a thread of
    /// its own. When that prints `koshi <version>` with a version other than
    /// the running one, the thread calls `on_other_version` with that version.
    ///
    /// Nothing runs while a version read runs, or when the file cannot be
    /// read. A file whose version read failed, or printed the running
    /// version, is not read again here until it changes, or until the instant
    /// [`schedule_restart_retry`](Self::schedule_restart_retry) set. Example: a
    /// rebuild that still prints `koshi 0.5.0-pr.1` is read once and restarts
    /// nothing. A version read that fails is logged at warning level.
    pub fn check_executable_file(
        self: &Arc<Self>,
        on_other_version: impl FnOnce(String) + Send + 'static,
    ) {
        let Some(file_identity) = ExecutableFileIdentity::load_from_path(&self.executable_path)
        else {
            return;
        };
        let watch_state = self.watch_state.lock().expect("executable watch");
        let is_examined_file = watch_state.examined_file_identity.as_ref() == Some(&file_identity);
        let is_restart_retry_due = watch_state
            .restart_retry_at
            .is_some_and(|restart_retry_at| Instant::now() >= restart_retry_at);
        if is_examined_file && !is_restart_retry_due {
            return;
        }
        self.start_version_read(watch_state, file_identity, on_other_version);
    }

    /// Run `<path> --version` on a thread of its own after this server refused
    /// a Hello for its protocol version, as
    /// [`check_executable_file`](Self::check_executable_file) does, whether or
    /// not that file was read before.
    ///
    /// Nothing runs while a version read runs, when the file cannot be read,
    /// or when the last read of the same file printed the running version.
    /// Example: a session whose read of a changed file failed once reads it
    /// again at the first Hello it refuses, and restarts into `0.6.0` when the
    /// file now prints `koshi 0.6.0`.
    pub fn check_executable_file_after_refused_hello(
        self: &Arc<Self>,
        on_other_version: impl FnOnce(String) + Send + 'static,
    ) {
        let Some(file_identity) = ExecutableFileIdentity::load_from_path(&self.executable_path)
        else {
            return;
        };
        let watch_state = self.watch_state.lock().expect("executable watch");
        let is_examined_file = watch_state.examined_file_identity.as_ref() == Some(&file_identity);
        if is_examined_file && watch_state.is_examined_file_on_running_version {
            return;
        }
        self.start_version_read(watch_state, file_identity, on_other_version);
    }

    /// Mark `file_identity` as the examined file and run `<path> --version` on
    /// a thread of its own, unless a version read already runs.
    ///
    /// The thread records whether the file printed the running version, calls
    /// `on_other_version` with any other version it printed, and logs a read
    /// that failed at warning level. A thread that cannot be started puts the
    /// examined identity and the restart retry instant back as they were.
    fn start_version_read(
        self: &Arc<Self>,
        mut watch_state: MutexGuard<'_, WatchState>,
        file_identity: ExecutableFileIdentity,
        on_other_version: impl FnOnce(String) + Send + 'static,
    ) {
        if watch_state.is_version_read_running {
            return;
        }
        watch_state.is_version_read_running = true;
        watch_state.is_examined_file_on_running_version = false;
        let previous_file_identity = watch_state.examined_file_identity.replace(file_identity);
        let previous_restart_retry_at = watch_state.restart_retry_at.take();
        drop(watch_state);
        let executable_watch = Arc::clone(self);
        let version_read_thread = std::thread::Builder::new()
            .name("koshi-version-read".to_string())
            .spawn(move || {
                let installed_version = read_installed_version(&executable_watch.executable_path);
                {
                    let mut watch_state = executable_watch
                        .watch_state
                        .lock()
                        .expect("executable watch");
                    watch_state.is_version_read_running = false;
                    watch_state.is_examined_file_on_running_version =
                        installed_version.as_deref().is_ok_and(|installed_version| {
                            installed_version == executable_watch.running_version
                        });
                }
                match installed_version {
                    Ok(installed_version)
                        if installed_version != executable_watch.running_version =>
                    {
                        on_other_version(installed_version);
                    }
                    Ok(_) => {}
                    Err(version_read_error) => {
                        tracing::warn!(
                            %version_read_error,
                            "the program file changed, and its version could not be read"
                        );
                    }
                }
            });
        if version_read_thread.is_err() {
            let mut watch_state = self.watch_state.lock().expect("executable watch");
            watch_state.is_version_read_running = false;
            watch_state.examined_file_identity = previous_file_identity;
            watch_state.restart_retry_at = previous_restart_retry_at;
        }
    }
}

/// The version `<executable_path> --version` prints, such as `0.5.0-pr.1`
/// for the line `koshi 0.5.0-pr.1`.
///
/// # Errors
/// Returns the sentence naming the binary and what is wrong: it could not be
/// run, it printed no line within [`VERSION_ANSWER_WAIT_DURATION`], or its
/// line is not `koshi <version>`.
pub fn read_installed_version(executable_path: &Path) -> Result<String, String> {
    let version_line =
        match read_first_output_line(executable_path, "--version", VERSION_ANSWER_WAIT_DURATION) {
            Ok(version_line) => version_line,
            Err(OutputLineError::NotStarted(process_spawn_error)) => {
                return Err(format!(
                    "the binary at {} could not be run: {process_spawn_error}",
                    executable_path.display()
                ));
            }
            Err(OutputLineError::NoLineInTime) => {
                return Err(format!(
                    "the binary at {} did not print its version within {} seconds",
                    executable_path.display(),
                    VERSION_ANSWER_WAIT_DURATION.as_secs()
                ));
            }
        };
    match version_line.trim().strip_prefix("koshi ") {
        Some(installed_version) if !installed_version.is_empty() => {
            Ok(installed_version.to_string())
        }
        _ => Err(format!(
            "the binary at {} printed {:?} for --version",
            executable_path.display(),
            version_line.trim()
        )),
    }
}

/// Why [`read_first_output_line`] has no line to hand back.
#[derive(Debug)]
pub enum OutputLineError {
    /// The program could not be started. Carries the failure.
    NotStarted(std::io::Error),
    /// The program printed no line within the wait.
    NoLineInTime,
}

/// Run the program at `executable_path` with the one argument
/// `program_argument` and no input, and hand back the first line it prints on
/// standard output, its newline included. A stream that ends before a newline
/// gives what it held.
///
/// A program file that another process holds open for writing (`ETXTBSY` on
/// Unix, `ERROR_SHARING_VIOLATION` on Windows) is started again every
/// `BUSY_PROGRAM_FILE_RETRY_INTERVAL_DURATION` (10 ms). A thread of its own reads
/// the line. The tries and the line together wait at most
/// `answer_wait_duration`, and a started program is ended and reaped either
/// way, which closes the pipe and ends that thread. A process the program
/// started that still holds its standard output keeps the pipe open, and the
/// thread reads on until that process closes it.
///
/// # Errors
/// [`OutputLineError::NotStarted`] for a program that could not be started,
/// with the last failure, and [`OutputLineError::NoLineInTime`] for one that
/// printed no line within `answer_wait_duration`.
pub fn read_first_output_line(
    executable_path: &Path,
    program_argument: &str,
    answer_wait_duration: Duration,
) -> Result<String, OutputLineError> {
    let answer_deadline = Instant::now() + answer_wait_duration;
    let mut child_process = loop {
        let process_spawn_error = match ProcessCommand::new(executable_path)
            .arg(program_argument)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child_process) => break child_process,
            Err(process_spawn_error) => process_spawn_error,
        };
        let is_program_file_busy = if cfg!(windows) {
            process_spawn_error.raw_os_error() == Some(WINDOWS_SHARING_VIOLATION_ERROR_CODE)
        } else {
            process_spawn_error.kind() == std::io::ErrorKind::ExecutableFileBusy
        };
        let is_retry_in_time =
            Instant::now() + BUSY_PROGRAM_FILE_RETRY_INTERVAL_DURATION < answer_deadline;
        if !is_program_file_busy || !is_retry_in_time {
            return Err(OutputLineError::NotStarted(process_spawn_error));
        }
        std::thread::sleep(BUSY_PROGRAM_FILE_RETRY_INTERVAL_DURATION);
    };
    let child_standard_output = child_process
        .stdout
        .take()
        .expect("the program was spawned with its standard output piped");
    let (output_line_sender, output_line_receiver) = mpsc::channel();
    let _ = std::thread::Builder::new()
        .name("koshi-output-line".to_string())
        .spawn(move || {
            let mut output_line = String::new();
            let _ = BufReader::new(child_standard_output).read_line(&mut output_line);
            let _ = output_line_sender.send(output_line);
        });
    let output_line_result = output_line_receiver
        .recv_timeout(answer_deadline.saturating_duration_since(Instant::now()));
    let _ = child_process.kill();
    let _ = child_process.wait();
    output_line_result.map_err(|_| OutputLineError::NoLineInTime)
}
