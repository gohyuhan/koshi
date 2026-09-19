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
//! The same module holds the address helpers every writer and reader shares:
//! [`compute_socket_address`](crate::endpoint::compute_socket_address) builds the control-socket
//! address a session listens on, and
//! [`remove_socket_file`](crate::endpoint::remove_socket_file) takes that
//! address off the disk once the session is gone.
//! [`compute_shared_socket_address`](crate::endpoint::compute_shared_socket_address) builds that
//! address for a session other local users may reach,
//! [`resolve_resume_file_path`](crate::endpoint::resolve_resume_file_path) names the file a session
//! replacing its own process image leaves its state in, and
//! [`resolve_advertisement_marker_path`](crate::endpoint::resolve_advertisement_marker_path),
//! [`write_advertisement_marker`](crate::endpoint::write_advertisement_marker) and
//! [`remove_advertisement_marker`](crate::endpoint::remove_advertisement_marker) handle the empty marker
//! file that names such a session on Windows.

use std::path::{Path, PathBuf};
use std::time::Duration;

use koshi_core::ids::SessionId;
use serde::{Deserialize, Serialize};

use crate::error::IpcError;
use crate::protocol::ConnectionToken;

/// The control-socket address a running `session` listens on: the string
/// [`Connection::connect`](crate::transport::Connection::connect) takes and
/// the [`EndpointFile`]'s `socket` field carries.
///
/// On Unix this is a socket-file path, `session-<uuid>.sock` directly inside
/// `runtime_directory` — the location [`validate_socket_address`](crate::validate::validate_socket_address)
/// accepts. On Windows it is the pipe name `koshi-session-<uuid>`, inside the
/// `koshi-` namespace that same check requires; `runtime_directory` goes unused
/// there.
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

/// The control-socket address a running `session` listens on when other local
/// users may reach it.
///
/// The same string [`compute_socket_address`] gives for `shared_user_directory`. On Unix this
/// is a socket-file path, `session-<uuid>.sock` directly inside
/// `shared_user_directory` — the location
/// [`validate_shared_socket_address`](crate::validate::validate_shared_socket_address)
/// accepts. On Windows it is the pipe name `koshi-session-<uuid>`: pipe names
/// share one machine-wide namespace, and `shared_user_directory` goes unused there.
#[must_use]
pub fn compute_shared_socket_address(
    shared_user_directory: &Path,
    session_id: SessionId,
) -> String {
    compute_socket_address(shared_user_directory, session_id)
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

/// Where the resume file for `session` lives: `session-<uuid>.resume`,
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

/// Where the marker advertising `session` machine-wide lives:
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
/// A marker that cannot be written is [`IpcError::AdvertWrite`] naming `advertisement_marker_path`.
pub fn write_advertisement_marker(advertisement_marker_path: &Path) -> Result<(), IpcError> {
    std::fs::write(advertisement_marker_path, b"").map_err(|io_error| IpcError::AdvertWrite {
        advert_marker_path: advertisement_marker_path.display().to_string(),
        error_detail: io_error.to_string(),
    })
}

/// Delete the marker at `advertisement_marker_path`. A path with nothing at it is left alone.
pub fn remove_advertisement_marker(advertisement_marker_path: &Path) {
    let _ = std::fs::remove_file(advertisement_marker_path);
}

/// Unlink the socket file at `socket_address` on Unix, where the address is a
/// filesystem path. A path with nothing at it is left alone. On Windows the
/// address is a pipe name, and nothing is removed.
pub fn remove_socket_file(socket_address: &str) {
    #[cfg(unix)]
    {
        let _ = std::fs::remove_file(socket_address);
    }
    #[cfg(windows)]
    {
        let _ = socket_address;
    }
}

/// What the endpoint file holds.
///
/// Decoding rejects any field it does not know: a misspelled name is an
/// error. The derived `Debug` prints the token as `ConnectionToken(***)`;
/// the real secret reaches only the file itself, through `Serialize`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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

impl EndpointFile {
    /// Where the endpoint file for `session` lives: `session-<uuid>.json`
    /// directly inside `runtime_directory`.
    ///
    /// Callers resolve `runtime_directory` through `koshi_paths::resolve_runtime_directory()`.
    #[must_use]
    pub fn resolve_endpoint_file_path(runtime_directory: &Path, session_id: SessionId) -> PathBuf {
        runtime_directory.join(format!("{session_id}.json"))
    }

    /// Write this endpoint file at `endpoint_file_path`, replacing whatever is there. A
    /// fresh file is created with mode `0600` on Unix.
    ///
    /// # Errors
    /// A file that cannot be encoded or written is
    /// [`IpcError::EndpointFileWrite`] naming `endpoint_file_path`.
    pub fn write_to_path(&self, endpoint_file_path: &Path) -> Result<(), IpcError> {
        let build_endpoint_file_write_error = |error_detail: String| IpcError::EndpointFileWrite {
            endpoint_file_path: endpoint_file_path.display().to_string(),
            error_detail,
        };
        let endpoint_file_bytes = serde_json::to_vec(self).map_err(|serialization_error| {
            build_endpoint_file_write_error(serialization_error.to_string())
        })?;
        koshi_storage::atomic::write_atomic(endpoint_file_path, &endpoint_file_bytes).map_err(
            |atomic_write_error| build_endpoint_file_write_error(atomic_write_error.to_string()),
        )
    }

    /// Read the endpoint file at `endpoint_file_path`.
    ///
    /// A path with no file is [`IpcError::EndpointFileMissing`]: no running
    /// Koshi has advertised a socket there. A file that cannot be read or
    /// whose bytes are not a readable endpoint file is
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
        serde_json::from_slice(&endpoint_file_bytes).map_err(|deserialization_error| {
            build_endpoint_file_unreadable_error(deserialization_error.to_string())
        })
    }
}

#[cfg(test)]
mod tests;
