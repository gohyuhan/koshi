//! The endpoint file: how a running Koshi advertises its control socket.
//!
//! Each running Koshi writes one JSON file — `session-<uuid>.json` — directly
//! inside the private (`0700`) runtime directory. The file names the
//! session's control-socket address, names the process advertising it, and
//! carries the [`ConnectionToken`](crate::protocol::ConnectionToken) a
//! connection from the same user presents at
//! [`Hello`](crate::protocol::IpcRequestKind::Hello). Only the user who
//! started Koshi can read the directory.
//!
//! The runtime writes the file when a session starts; the `koshi` CLI reads
//! it to find the socket and the token before connecting. Writes go through
//! [`koshi_storage::atomic::write_atomic`]: a reader finds the old content
//! or the new, never a half-written middle.
//!
//! Beside it, each server writes a
//! [`ServerProgramFile`](crate::endpoint::ServerProgramFile): the koshi
//! version it runs and the program file it restarts into. The `koshi` CLI reads it
//! when a server refuses this build's protocol version.
//!
//! The same module holds the address helpers every writer and reader shares:
//! [`compute_socket_address`](crate::endpoint::compute_socket_address) builds the control-socket
//! address a session listens on, and [`delete_socket_file`](crate::endpoint::delete_socket_file)
//! takes that address off the disk once the session is gone.
//! [`resolve_resume_file_path`](crate::endpoint::resolve_resume_file_path) names the file a session
//! replacing its own process image leaves its state in,
//! [`is_replacing_its_image`](crate::endpoint::is_replacing_its_image) reads whether that file
//! is younger than the restart window,
//! [`resolve_update_lock_path`](crate::endpoint::resolve_update_lock_path) names the file
//! `koshi update` locks while it restarts the running servers,
//! [`is_update_restarting_servers`](crate::endpoint::is_update_restarting_servers) reads whether
//! that lock is held, and
//! [`resolve_advertisement_marker_path`](crate::endpoint::resolve_advertisement_marker_path),
//! [`write_advertisement_marker`](crate::endpoint::write_advertisement_marker) and
//! [`delete_advertisement_marker`](crate::endpoint::delete_advertisement_marker) handle the empty
//! marker file that names such a session on Windows.

use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use koshi_core::ids::SessionId;
use koshi_host::process_tree::{find_process_record, ProcessRecord};
use serde::{Deserialize, Serialize};

use crate::error::IpcError;
use crate::protocol::ConnectionToken;
use crate::remote_state::find_format_mismatch;
use crate::wire::has_exactly_json_fields;

/// The format number this build writes into an endpoint file, and the
/// highest one it reads.
///
/// The value and the rule it follows live in
/// [`koshi_core::compat::ENDPOINT_FILE_FORMAT`].
pub const ENDPOINT_FILE_FORMAT: u32 = koshi_core::compat::ENDPOINT_FILE_FORMAT.maximum_version;

/// The format number this build writes into a program file, and the only one
/// it reads.
///
/// The value and the rule it follows live in
/// [`koshi_core::compat::PROGRAM_FILE_FORMAT`].
pub const PROGRAM_FILE_FORMAT: u32 = koshi_core::compat::PROGRAM_FILE_FORMAT.maximum_version;

/// The control-socket address a running `session_id` listens on: the string
/// [`Connection::connect`](crate::transport::Connection::connect) takes and
/// the [`EndpointFile`]'s `socket_address` field carries.
///
/// On Unix this is a socket-file path, `session-<uuid>.sock` directly inside
/// `runtime_directory`: the location
/// [`validate_socket_address`](crate::validate::validate_socket_address)
/// accepts. Passing a session's shared user directory as `runtime_directory`
/// gives the address other local users reach, the location
/// [`validate_shared_socket_address`](crate::validate::validate_shared_socket_address)
/// accepts. On Windows it is the pipe name `koshi-session-<uuid>`, inside the
/// `koshi-` namespace that same check requires; pipe names share one
/// machine-wide namespace, and `runtime_directory` goes unused there.
///
/// Every consumer derives the address through here, including the
/// `KOSHI_SOCKET` variable injected into spawned panes.
#[must_use]
pub fn compute_socket_address(runtime_directory: &Path, session_id: SessionId) -> String {
    #[cfg(unix)]
    {
        runtime_directory
            .join(format!("{session_id}.sock"))
            .display()
            .to_string()
    }
    #[cfg(windows)]
    {
        let _ = runtime_directory;
        format!("koshi-{session_id}")
    }
}

/// What a resume file's name ends in, after the session id. Every reader that
/// walks a directory for resume files matches on this.
pub const RESUME_SUFFIX: &str = ".resume";

/// How long a session replacing its own image has to come back.
///
/// Three sides read it. The session server writes the resume file and swaps;
/// the router leaves a session whose file is younger than this alone, and
/// removes the session when it is older; an attached client waits this long
/// for the new socket before it gives up and reports the session gone.
pub const RESTART_WINDOW_DURATION: Duration = Duration::from_secs(30);

/// Where the resume file for `session_id` lives: `session-<uuid>.resume`,
/// directly beside that session's endpoint file inside `runtime_directory`.
///
/// A session server about to replace its own process image writes there the
/// state its next image takes back; the new image reads that state and deletes
/// the file. The router reads the same path to tell a session that is replacing
/// its image from one that stopped answering, and removes a file no session
/// claims any more.
#[must_use]
pub fn resolve_resume_file_path(runtime_directory: &Path, session_id: SessionId) -> PathBuf {
    runtime_directory.join(format!("{session_id}{RESUME_SUFFIX}"))
}

/// Whether `session_id` is replacing its own process image right now: its
/// resume file in `runtime_directory` exists and is younger than
/// [`RESTART_WINDOW_DURATION`]. A file stamped ahead of this machine's clock
/// reads as younger. A file as old as the window, or older, reads as a swap
/// that died.
///
/// Example: a resume file written 2 seconds ago gives `true`, one written 30
/// seconds ago gives `false`, and no resume file gives `false`.
#[must_use]
pub fn is_replacing_its_image(runtime_directory: &Path, session_id: SessionId) -> bool {
    let Ok(resume_file_modified_at) =
        std::fs::metadata(resolve_resume_file_path(runtime_directory, session_id))
            .and_then(|resume_file_metadata| resume_file_metadata.modified())
    else {
        return false;
    };
    resume_file_modified_at
        .elapsed()
        .map_or(true, |resume_file_age| {
            resume_file_age < RESTART_WINDOW_DURATION
        })
}

/// Where `koshi update` holds its lock while it restarts the running sessions
/// and the router: `update.lock`, directly inside `runtime_directory`.
#[must_use]
pub fn resolve_update_lock_path(runtime_directory: &Path) -> PathBuf {
    runtime_directory.join("update.lock")
}

/// Whether a `koshi update` holds the lock at
/// [`resolve_update_lock_path`] in `runtime_directory` right now: a shared
/// lock of that file would block. A file that is missing or cannot be opened,
/// and a file this process can share-lock, give `false`.
#[must_use]
pub fn is_update_restarting_servers(runtime_directory: &Path) -> bool {
    let Ok(update_lock_file) = std::fs::File::open(resolve_update_lock_path(runtime_directory))
    else {
        return false;
    };
    matches!(
        update_lock_file.try_lock_shared(),
        Err(std::fs::TryLockError::WouldBlock)
    )
}

/// Where the marker advertising `session_id` machine-wide lives:
/// `session-<uuid>`, with no extension, directly inside `shared_directory`.
///
/// On Windows, a client lists these markers to learn which sessions listen
/// on a pipe. The marker carries no bytes: the pipe name follows from the
/// session id the file is named after.
#[must_use]
pub fn resolve_advertisement_marker_path(
    shared_directory: &Path,
    session_id: SessionId,
) -> PathBuf {
    shared_directory.join(session_id.to_string())
}

/// Write the marker at `advertisement_marker_path` as an empty file, replacing whatever is there.
///
/// # Errors
/// A marker that cannot be written is [`IpcError::AdvertisementMarkerWrite`] naming
/// `advertisement_marker_path`.
pub fn write_advertisement_marker(advertisement_marker_path: &Path) -> Result<(), IpcError> {
    std::fs::write(advertisement_marker_path, b"").map_err(|io_error| {
        IpcError::AdvertisementMarkerWrite {
            advertisement_marker_path: advertisement_marker_path.display().to_string(),
            error_detail: io_error.to_string(),
        }
    })
}

/// Delete the marker at `advertisement_marker_path`. A path with nothing at it is left alone.
pub fn delete_advertisement_marker(advertisement_marker_path: &Path) {
    let _ = std::fs::remove_file(advertisement_marker_path);
}

/// Unlink the socket file at `socket_address` on Unix, where the address is a
/// filesystem path. A path with nothing at it is left alone. On Windows the
/// address is a pipe name, and nothing is removed.
pub fn delete_socket_file(socket_address: &str) {
    #[cfg(unix)]
    {
        let _ = std::fs::remove_file(socket_address);
    }
    #[cfg(windows)]
    {
        let _ = socket_address;
    }
}

/// The running process `process_id`, when it started in the whole second of
/// the modification time of the endpoint file at `endpoint_file_path`, or
/// earlier.
///
/// Example: a session started at `1000.700000` s writes its file at
/// `1000.900000` s, and a filesystem that keeps whole seconds stores
/// `1000` s; both find the process. A process with the same id that started
/// at `1001.000000` s or after gives `None`.
///
/// `None` also for `0`, for an id no running process has, for a zombie, for
/// a process this process cannot read, and for an endpoint file whose
/// modification time cannot be read or lies before `UNIX_EPOCH`.
#[must_use]
pub fn find_endpoint_process_record(
    endpoint_file_path: &Path,
    process_id: u32,
) -> Option<ProcessRecord> {
    let endpoint_written_at = std::fs::metadata(endpoint_file_path)
        .and_then(|endpoint_metadata| endpoint_metadata.modified())
        .ok()?;
    let endpoint_written_at_seconds = endpoint_written_at
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_secs();
    let process_record = find_process_record(process_id)?;
    let started_at_seconds = process_record
        .started_at
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_secs();
    (started_at_seconds <= endpoint_written_at_seconds).then_some(process_record)
}

/// Whether the process `process_id`, which the endpoint file at
/// `endpoint_file_path` names, still runs as the session that wrote that file.
/// A caller asks after that session's socket refused a connect.
///
/// On macOS: `true` when [`find_endpoint_process_record`] finds the process.
/// [`EndpointFile::write_to_path`] sets the file's modification time to the
/// start time of the process the file names. On macOS a Unix socket whose
/// listen queue holds 3/2 of its limit refuses a connect.
///
/// On Linux and Windows: `false`. A Linux Unix socket refuses a connect only
/// when nothing listens on it, and fails one with `EAGAIN` while its listen
/// queue is full. A Windows pipe connect reports nothing listening only when
/// no instance of the pipe exists.
#[must_use]
pub fn is_refusal_from_live_session(endpoint_file_path: &Path, process_id: u32) -> bool {
    if cfg!(target_os = "macos") {
        find_endpoint_process_record(endpoint_file_path, process_id).is_some()
    } else {
        false
    }
}

/// What the endpoint file holds.
///
/// On disk the fields sit beside `"file_format":` [`ENDPOINT_FILE_FORMAT`].
/// Decoding skips a field it does not know. The derived `Debug` prints the
/// token as `ConnectionToken(***)`; the real secret reaches only the file
/// itself, through `Serialize`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointFile {
    /// The control-socket address: a socket-file path on Unix, a bare pipe
    /// name on Windows — the string
    /// [`Connection::connect`](crate::transport::Connection::connect) takes.
    pub socket_address: String,
    /// The secret a connection presents at Hello.
    pub connection_token: ConnectionToken,
    /// The process id of the process advertising this socket.
    pub process_id: u32,
}

/// An endpoint file as this build writes it: [`ENDPOINT_FILE_FORMAT`] beside
/// the fields of `endpoint_file`.
#[derive(Serialize)]
struct EndpointFileRecord<'endpoint> {
    file_format: u32,
    #[serde(flatten)]
    endpoint_file: &'endpoint EndpointFile,
}

/// A program file as this build writes it: [`PROGRAM_FILE_FORMAT`] beside the
/// fields of `server_program_file`.
#[derive(Serialize)]
struct ServerProgramFileRecord<'program> {
    file_format: u32,
    #[serde(flatten)]
    server_program_file: &'program ServerProgramFile,
}

/// The `file_format` field of a runtime file, read before the rest of it.
/// `None` when the file has no such field. Every other field is skipped.
#[derive(Deserialize)]
struct FileFormatField {
    file_format: Option<u32>,
}

/// The format 1 endpoint file `v0.2.0-pr.1` to `v0.4.0` write: `{socket,
/// token, pid}`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FormatOneShortNamedEndpointFile {
    socket: String,
    token: ConnectionToken,
    pid: u32,
}

/// Decode `endpoint_file_bytes` as an endpoint file of any format this build
/// reads.
///
/// - `"file_format": 2` decodes the fields beside it.
/// - No `file_format` field is format 1: the `v0.5.0-pr.1` shape
///   `{socket_address, connection_token, process_id}`, or the `v0.4.0` shape
///   `{socket, token, pid}`, which becomes the same three fields.
///
/// # Errors
/// The sentence naming what is wrong: bytes that are not a JSON object, a
/// format other than [`ENDPOINT_FILE_FORMAT`] (`format 3 is not the 2 this
/// build reads`), or fields that fit neither format 1 shape.
fn parse_endpoint_file(endpoint_file_bytes: &[u8]) -> Result<EndpointFile, String> {
    let file_format_field: FileFormatField = serde_json::from_slice(endpoint_file_bytes)
        .map_err(|deserialization_error| deserialization_error.to_string())?;
    if let Some(file_format) = file_format_field.file_format {
        if let Some(format_mismatch) = find_format_mismatch(file_format, ENDPOINT_FILE_FORMAT) {
            return Err(format_mismatch);
        }
        return serde_json::from_slice(endpoint_file_bytes)
            .map_err(|deserialization_error| deserialization_error.to_string());
    }
    let format_one_error = match serde_json::from_slice(endpoint_file_bytes) {
        Ok(endpoint_file) => return Ok(endpoint_file),
        Err(deserialization_error) => deserialization_error.to_string(),
    };
    match serde_json::from_slice::<FormatOneShortNamedEndpointFile>(endpoint_file_bytes) {
        Ok(short_named_endpoint_file) => Ok(EndpointFile {
            socket_address: short_named_endpoint_file.socket,
            connection_token: short_named_endpoint_file.token,
            process_id: short_named_endpoint_file.pid,
        }),
        Err(_) => Err(format_one_error),
    }
}

impl EndpointFile {
    /// Where the endpoint file for `session_id` lives: `session-<uuid>.json`
    /// directly inside `runtime_directory`.
    ///
    /// Callers resolve `runtime_directory` through `koshi_paths::resolve_runtime_directory()`.
    #[must_use]
    pub fn resolve_endpoint_file_path(runtime_directory: &Path, session_id: SessionId) -> PathBuf {
        runtime_directory.join(format!("{session_id}.json"))
    }

    /// Write this endpoint file at `endpoint_file_path` in format
    /// [`ENDPOINT_FILE_FORMAT`], replacing whatever is there. A fresh file is
    /// created with mode `0600` on Unix.
    ///
    /// On macOS the written file's modification time is then set to the start
    /// time `proc_pidinfo` gives for `process_id`, which
    /// [`is_refusal_from_live_session`] compares. A process `proc_pidinfo`
    /// cannot read, and a time that cannot be set, leave the time of the write.
    ///
    /// # Errors
    /// A file that cannot be encoded or written is
    /// [`IpcError::EndpointFileWrite`] naming `endpoint_file_path`.
    pub fn write_to_path(&self, endpoint_file_path: &Path) -> Result<(), IpcError> {
        let build_endpoint_file_write_error = |error_detail: String| IpcError::EndpointFileWrite {
            endpoint_file_path: endpoint_file_path.display().to_string(),
            error_detail,
        };
        let endpoint_file_bytes = serde_json::to_vec(&EndpointFileRecord {
            file_format: ENDPOINT_FILE_FORMAT,
            endpoint_file: self,
        })
        .map_err(|serialization_error| {
            build_endpoint_file_write_error(serialization_error.to_string())
        })?;
        koshi_storage::atomic::write_atomic(endpoint_file_path, &endpoint_file_bytes).map_err(
            |atomic_write_error| build_endpoint_file_write_error(atomic_write_error.to_string()),
        )?;
        #[cfg(target_os = "macos")]
        {
            if let Some(process_record) = find_process_record(self.process_id) {
                if let Ok(endpoint_file) = std::fs::File::options()
                    .write(true)
                    .open(endpoint_file_path)
                {
                    let _ = endpoint_file.set_modified(process_record.started_at);
                }
            }
        }
        Ok(())
    }

    /// Read the endpoint file at `endpoint_file_path`, in format
    /// [`ENDPOINT_FILE_FORMAT`], or in format 1, which it converts: the
    /// `v0.5.0-pr.1` shape `{socket_address, connection_token, process_id}`, or
    /// the shape `v0.2.0-pr.1` to `v0.4.0` write, `{socket, token, pid}`.
    ///
    /// A path with no file is [`IpcError::EndpointFileMissing`]: no running
    /// Koshi has advertised a socket there. A file that holds the fields
    /// `{socket, token}` alone, which a koshi 0.1.0 window writes, is
    /// [`IpcError::Koshi010WindowEndpointFile`]. Any other file that cannot be
    /// read, or whose bytes fit neither format, is
    /// [`IpcError::EndpointFileUnreadable`].
    pub fn load_from_path(endpoint_file_path: &Path) -> Result<EndpointFile, IpcError> {
        let build_endpoint_file_unreadable_error =
            |error_detail: String| IpcError::EndpointFileUnreadable {
                endpoint_file_path: endpoint_file_path.display().to_string(),
                error_detail,
            };
        let endpoint_file_bytes = std::fs::read(endpoint_file_path).map_err(|read_error| {
            if read_error.kind() == std::io::ErrorKind::NotFound {
                IpcError::EndpointFileMissing {
                    endpoint_file_path: endpoint_file_path.display().to_string(),
                }
            } else {
                build_endpoint_file_unreadable_error(read_error.to_string())
            }
        })?;
        parse_endpoint_file(&endpoint_file_bytes).map_err(|error_detail| {
            if has_exactly_json_fields(&endpoint_file_bytes, &["socket", "token"]) {
                IpcError::Koshi010WindowEndpointFile {
                    endpoint_file_path: endpoint_file_path.display().to_string(),
                }
            } else {
                build_endpoint_file_unreadable_error(error_detail)
            }
        })
    }
}

/// What a server's program file holds: the koshi version the server runs and
/// the program file it restarts into, with the id of the process that wrote
/// it.
///
/// A session writes `session-<uuid>.program` and the router writes
/// `router.program`, each directly inside the runtime directory, after its
/// endpoint file and before it accepts a connection. Each removes the file
/// before its endpoint file when it stops serving. On disk the fields sit
/// beside `"file_format":` [`PROGRAM_FILE_FORMAT`]. Decoding skips a field it
/// does not know.
///
/// Example: `{"file_format":1,"process_id":5000,"build_version":"0.6.0","program_path":"/usr/local/bin/koshi"}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerProgramFile {
    /// The id of the server process that wrote the file.
    pub process_id: u32,
    /// The koshi version the server runs, such as `0.6.0`.
    pub build_version: String,
    /// The path of the program file the server restarts into, with its
    /// symbolic links kept. A path that is not valid UTF-8 holds U+FFFD in
    /// place of each byte sequence that is not.
    pub program_path: String,
}

impl ServerProgramFile {
    /// Where the program file for `session_id` lives: `session-<uuid>.program`
    /// directly inside `runtime_directory`.
    #[must_use]
    pub fn resolve_session_program_file_path(
        runtime_directory: &Path,
        session_id: SessionId,
    ) -> PathBuf {
        runtime_directory.join(format!("{session_id}.program"))
    }

    /// Write this program file at `program_file_path` in format
    /// [`PROGRAM_FILE_FORMAT`], replacing whatever is there. A fresh file is
    /// created with mode `0600` on Unix.
    ///
    /// # Errors
    /// A file that cannot be encoded or written is
    /// [`IpcError::ProgramFileWrite`] naming `program_file_path`.
    pub fn write_to_path(&self, program_file_path: &Path) -> Result<(), IpcError> {
        let build_program_file_write_error = |error_detail: String| IpcError::ProgramFileWrite {
            program_file_path: program_file_path.display().to_string(),
            error_detail,
        };
        let program_file_bytes = serde_json::to_vec(&ServerProgramFileRecord {
            file_format: PROGRAM_FILE_FORMAT,
            server_program_file: self,
        })
        .map_err(|serialization_error| {
            build_program_file_write_error(serialization_error.to_string())
        })?;
        koshi_storage::atomic::write_atomic(program_file_path, &program_file_bytes).map_err(
            |atomic_write_error| build_program_file_write_error(atomic_write_error.to_string()),
        )
    }

    /// Read the program file at `program_file_path`. `None` when no file is
    /// there.
    ///
    /// # Errors
    /// A file that cannot be read, whose bytes are not a JSON object, whose
    /// `file_format` is missing or is not [`PROGRAM_FILE_FORMAT`], or whose
    /// fields are not a program file's, is [`IpcError::ProgramFileUnreadable`]
    /// naming `program_file_path`. Example detail for a file a newer build
    /// wrote: `format 2 is not the 1 this build reads`.
    pub fn load_from_path(program_file_path: &Path) -> Result<Option<ServerProgramFile>, IpcError> {
        let build_program_file_unreadable_error =
            |error_detail: String| IpcError::ProgramFileUnreadable {
                program_file_path: program_file_path.display().to_string(),
                error_detail,
            };
        let program_file_bytes = match std::fs::read(program_file_path) {
            Ok(program_file_bytes) => program_file_bytes,
            Err(read_error) if read_error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(None);
            }
            Err(read_error) => {
                return Err(build_program_file_unreadable_error(read_error.to_string()))
            }
        };
        let file_format_field: FileFormatField = serde_json::from_slice(&program_file_bytes)
            .map_err(|deserialization_error| {
                build_program_file_unreadable_error(deserialization_error.to_string())
            })?;
        let Some(file_format) = file_format_field.file_format else {
            return Err(build_program_file_unreadable_error(
                "it has no file_format field".to_string(),
            ));
        };
        if let Some(format_mismatch) = find_format_mismatch(file_format, PROGRAM_FILE_FORMAT) {
            return Err(build_program_file_unreadable_error(format_mismatch));
        }
        serde_json::from_slice(&program_file_bytes)
            .map(Some)
            .map_err(|deserialization_error| {
                build_program_file_unreadable_error(deserialization_error.to_string())
            })
    }
}

#[cfg(test)]
mod tests;
