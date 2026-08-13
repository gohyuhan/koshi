//! The endpoint file: how a running Koshi advertises its control socket.
//!
//! Each running Koshi writes one JSON file — `session-<uuid>.json` — directly
//! inside the private (`0700`) runtime directory. The file names the
//! session's control-socket address, names the process advertising it, and
//! carries the [`ConnectionToken`](crate::protocol::ConnectionToken) a
//! connection from the same user presents at
//! [`Hello`](crate::protocol::IpcRequestKind::Hello). The directory is
//! readable only by the user who started Koshi, so being able to read the
//! file is itself the same-user proof.
//!
//! The runtime writes the file when a session starts; the `koshi` CLI reads
//! it to find the socket and the token before connecting. Writes go through
//! [`koshi_storage::atomic::write_atomic`], so a reader finds the old content
//! or the new, never a half-written middle.
//!
//! The same module holds the address helpers every writer and reader shares:
//! [`socket_addr`](crate::endpoint::socket_addr) builds the control-socket
//! address a session listens on, and
//! [`remove_socket_file`](crate::endpoint::remove_socket_file) takes that
//! address off the disk once the session is gone.
//! [`shared_socket_addr`](crate::endpoint::shared_socket_addr) builds that
//! address for a session other local users may reach,
//! [`resume_path`](crate::endpoint::resume_path) names the file a session
//! replacing its own process image leaves its state in, and
//! [`advert_path`](crate::endpoint::advert_path),
//! [`write_advert`](crate::endpoint::write_advert) and
//! [`remove_advert`](crate::endpoint::remove_advert) handle the empty marker
//! file that names such a session on Windows.

use std::path::{Path, PathBuf};

use koshi_core::ids::SessionId;
use serde::{Deserialize, Serialize};

use crate::error::IpcError;
use crate::protocol::ConnectionToken;

/// The control-socket address a running `session` listens on: the string
/// [`Connection::connect`](crate::transport::Connection::connect) takes and
/// the [`EndpointFile`]'s `socket` field carries.
///
/// On Unix this is a socket-file path, `session-<uuid>.sock` directly inside
/// `runtime_dir` — the location [`validate_socket_addr`](crate::validate::validate_socket_addr)
/// accepts. On Windows it is the pipe name `koshi-session-<uuid>`, inside the
/// `koshi-` namespace that same check requires; a pipe has no filesystem
/// path, so `runtime_dir` goes unused there.
///
/// Every consumer derives the address through here, including the
/// `KOSHI_SOCKET` variable injected into spawned panes, so they all name the
/// same place.
#[must_use]
pub fn socket_addr(runtime_dir: &Path, session: SessionId) -> String {
    #[cfg(unix)]
    {
        runtime_dir
            .join(format!("{session}.sock"))
            .display()
            .to_string()
    }
    #[cfg(windows)]
    {
        let _ = runtime_dir;
        format!("koshi-{session}")
    }
}

/// The control-socket address a running `session` listens on when other local
/// users may reach it.
///
/// On Unix this is a socket-file path, `session-<uuid>.sock` directly inside
/// `shared_user_dir` — the location
/// [`validate_shared_socket_addr`](crate::validate::validate_shared_socket_addr)
/// accepts. On Windows it is the pipe name [`socket_addr`] gives,
/// `koshi-session-<uuid>`: pipe names share one machine-wide namespace, so
/// `shared_user_dir` goes unused there.
#[must_use]
pub fn shared_socket_addr(shared_user_dir: &Path, session: SessionId) -> String {
    #[cfg(unix)]
    {
        shared_user_dir
            .join(format!("{session}.sock"))
            .display()
            .to_string()
    }
    #[cfg(windows)]
    {
        let _ = shared_user_dir;
        format!("koshi-{session}")
    }
}

/// What a resume file's name ends in, after the session id. Every reader that
/// walks a directory for resume files matches on this, so the name is built and
/// recognised from one place.
pub const RESUME_SUFFIX: &str = ".resume";

/// Where the resume file for `session` lives: `session-<uuid>.resume`,
/// directly beside that session's endpoint file inside `runtime_dir`.
///
/// A session server about to replace its own process image writes there the
/// state its next image takes back; the new image reads that state and deletes
/// the file. The router reads the same path to tell a session that is replacing
/// its image from one that stopped answering, and removes a file no session
/// claims any more.
#[must_use]
pub fn resume_path(runtime_dir: &Path, session: SessionId) -> PathBuf {
    runtime_dir.join(format!("{session}{RESUME_SUFFIX}"))
}

/// Where the marker advertising `session` machine-wide lives:
/// `session-<uuid>`, with no extension, directly inside `shared_dir`.
///
/// A Windows pipe has no filesystem location, so this marker is what tells a
/// client which sessions listen on one. It carries no bytes: the pipe name
/// follows from the session id the file is named after.
#[must_use]
pub fn advert_path(shared_dir: &Path, session: SessionId) -> PathBuf {
    shared_dir.join(session.to_string())
}

/// Write the marker at `path` as an empty file, replacing whatever is there.
pub fn write_advert(path: &Path) -> Result<(), IpcError> {
    std::fs::write(path, b"").map_err(|error| IpcError::EndpointFileWrite {
        path: path.display().to_string(),
        detail: error.to_string(),
    })
}

/// Delete the marker at `path`. A path with nothing at it is left alone.
pub fn remove_advert(path: &Path) {
    let _ = std::fs::remove_file(path);
}

/// Unlink the socket file at `addr` on Unix, where the address is a
/// filesystem path. On Windows the address is a pipe name that vanishes with
/// its last handle, so there is nothing to remove.
pub fn remove_socket_file(addr: &str) {
    #[cfg(unix)]
    {
        let _ = std::fs::remove_file(addr);
    }
    #[cfg(windows)]
    {
        let _ = addr;
    }
}

/// What the endpoint file holds.
///
/// Decoding rejects any field it does not know, so a misspelled name is an
/// error. The derived `Debug` prints the token as `***`; the real secret
/// reaches only the file itself, through `Serialize`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EndpointFile {
    /// The control-socket address: a socket-file path on Unix, a bare pipe
    /// name on Windows — the string
    /// [`Connection::connect`](crate::transport::Connection::connect) takes.
    pub socket: String,
    /// The secret a connection presents at Hello.
    pub token: ConnectionToken,
    /// The process id of the process advertising this socket.
    pub pid: u32,
}

impl EndpointFile {
    /// Where the endpoint file for `session` lives: `session-<uuid>.json`
    /// directly inside `runtime_dir`.
    ///
    /// Callers resolve `runtime_dir` through `koshi_paths::runtime_dir()`.
    #[must_use]
    pub fn path(runtime_dir: &Path, session: SessionId) -> PathBuf {
        runtime_dir.join(format!("{session}.json"))
    }

    /// Write this endpoint file at `path`, replacing whatever is there.
    pub fn write(&self, path: &Path) -> Result<(), IpcError> {
        let write_failed = |detail: String| IpcError::EndpointFileWrite {
            path: path.display().to_string(),
            detail,
        };
        let data = serde_json::to_vec(self).map_err(|error| write_failed(error.to_string()))?;
        koshi_storage::atomic::write_atomic(path, &data)
            .map_err(|error| write_failed(error.to_string()))
    }

    /// Read the endpoint file at `path`.
    ///
    /// A path with no file is [`IpcError::EndpointFileMissing`]: no running
    /// Koshi has advertised a socket there. A file that cannot be read or
    /// whose bytes are not a readable endpoint file is
    /// [`IpcError::EndpointFileUnreadable`].
    pub fn read(path: &Path) -> Result<EndpointFile, IpcError> {
        let unreadable = |detail: String| IpcError::EndpointFileUnreadable {
            path: path.display().to_string(),
            detail,
        };
        let data = std::fs::read(path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                IpcError::EndpointFileMissing {
                    path: path.display().to_string(),
                }
            } else {
                unreadable(error.to_string())
            }
        })?;
        serde_json::from_slice(&data).map_err(|error| unreadable(error.to_string()))
    }
}

#[cfg(test)]
mod tests;
