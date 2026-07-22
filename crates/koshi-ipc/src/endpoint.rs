//! The endpoint file: how a running Koshi advertises its control socket.
//!
//! Each running Koshi writes one JSON file — `session-<uuid>.json` — directly
//! inside the private (`0700`) runtime directory. The file names the
//! session's control-socket address and carries the
//! [`ConnectionToken`](crate::protocol::ConnectionToken) a connection must
//! present at
//! [`Hello`](crate::protocol::IpcRequestKind::Hello). The directory is
//! readable only by the user who started Koshi, so being able to read the
//! file is itself the same-user proof.
//!
//! The runtime writes the file when a session starts; the `koshi` CLI reads
//! it to find the socket and the token before connecting. Writes go through
//! [`koshi_storage::atomic::write_atomic`], so a reader finds the old content
//! or the new, never a half-written middle.

use std::path::{Path, PathBuf};

use koshi_core::ids::SessionId;
use serde::{Deserialize, Serialize};

use crate::error::IpcError;
use crate::protocol::ConnectionToken;

/// What the endpoint file holds.
///
/// Decoding rejects any field it does not know, so a misspelled name is an
/// error rather than a field that quietly keeps its default. The derived
/// `Debug` prints the token as `***`; the real secret reaches only the file
/// itself, through `Serialize`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EndpointFile {
    /// The control-socket address: a socket-file path on Unix, a bare pipe
    /// name on Windows — the string
    /// [`Connection::connect`](crate::transport::Connection::connect) takes.
    pub socket: String,
    /// The secret a connection presents at Hello.
    pub token: ConnectionToken,
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
        let data = std::fs::read(path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                IpcError::EndpointFileMissing {
                    path: path.display().to_string(),
                }
            } else {
                IpcError::EndpointFileUnreadable {
                    path: path.display().to_string(),
                    detail: error.to_string(),
                }
            }
        })?;
        serde_json::from_slice(&data).map_err(|error| IpcError::EndpointFileUnreadable {
            path: path.display().to_string(),
            detail: error.to_string(),
        })
    }
}

#[cfg(test)]
mod tests;
