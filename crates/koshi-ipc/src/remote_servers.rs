//! The servers a user has connected to, saved on the dialling user's own
//! machine, so nothing is retyped on the next connection.
//!
//! One [`SavedServer`](crate::remote_servers::SavedServer) holds the address,
//! the secret the operator handed out, the fingerprint of the certificate
//! that server presented on the first connection, and an optional name the
//! user chose. After the first connection the user types the name, not the
//! address.
//!
//! The whole set lives in one JSON file —
//! [`store_path`](crate::remote_servers::store_path) — inside the private
//! koshi data directory. The file carries the format number
//! [`SERVER_STORE_FORMAT`](crate::remote_servers::SERVER_STORE_FORMAT), and a
//! file carrying any other number is refused. Writes go through
//! [`koshi_storage::atomic::write_atomic`], so a reader finds the old content
//! or the new, never a half-written middle.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::error::{IpcError, RemoteFile};
use crate::protocol::ConnectionToken;

/// The format number this build writes into every saved-server file, and the
/// only one it reads back.
///
/// The value and the rule it follows live in
/// [`koshi_core::compat::SAVED_SERVER_FORMAT`].
pub const SERVER_STORE_FORMAT: u32 = koshi_core::compat::SAVED_SERVER_FORMAT.max;

/// One server this machine has connected to.
///
/// Decoding rejects any field it does not know, so a misspelled name is an
/// error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedServer {
    /// The name the user chose for this server, or `None` when they chose
    /// none. The user types it in place of the address.
    pub name: Option<String>,
    /// Where the server listens, as `host:port`.
    pub address: String,
    /// The secret the operator handed out with a grant. `ConnectionToken`'s
    /// `Debug` and `Display` write it redacted.
    pub secret: ConnectionToken,
    /// The sha256 of the certificate this server presented on the first
    /// connection, as 64 lowercase hex characters. A later connection that
    /// presents a different one is refused.
    pub fingerprint: String,
    /// When this server was first saved.
    pub added_at: SystemTime,
    /// When a connection to this server last opened, or `None` when none has
    /// since it was saved.
    pub last_used_at: Option<SystemTime>,
}

/// Every server this machine has connected to.
///
/// Decoding rejects any field it does not know, so a misspelled name is an
/// error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerStore {
    /// The format number of the file these records came from or go to.
    pub format: u32,
    /// One record per server, in the order they were first saved.
    pub records: Vec<SavedServer>,
}

impl Default for ServerStore {
    fn default() -> Self {
        ServerStore::new()
    }
}

impl ServerStore {
    /// An empty store at the format number this build writes.
    #[must_use]
    pub fn new() -> ServerStore {
        ServerStore {
            format: SERVER_STORE_FORMAT,
            records: Vec::new(),
        }
    }

    /// Read the store at `path`.
    ///
    /// A path with no file is an empty store: this machine has connected to
    /// nothing yet. A file that cannot be read, whose bytes are not a
    /// readable store, or whose format number is not
    /// [`SERVER_STORE_FORMAT`] is [`IpcError::RemoteFileUnreadable`].
    pub fn read(path: &Path) -> Result<ServerStore, IpcError> {
        let unreadable = |detail: String| IpcError::RemoteFileUnreadable {
            file: RemoteFile::SavedServers,
            path: path.display().to_string(),
            detail,
        };
        let data = match std::fs::read(path) {
            Ok(data) => data,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ServerStore::new())
            }
            Err(error) => return Err(unreadable(error.to_string())),
        };
        let store: ServerStore =
            serde_json::from_slice(&data).map_err(|error| unreadable(error.to_string()))?;
        if store.format != SERVER_STORE_FORMAT {
            return Err(unreadable(format!(
                "format {} is not the {SERVER_STORE_FORMAT} this build reads",
                store.format
            )));
        }
        Ok(store)
    }

    /// Write this store at `path`, replacing whatever is there, and create
    /// the directory holding it when it is missing.
    ///
    /// The file is restricted to the owning user: mode `0600` on Unix, set on
    /// an existing file before the replace so the new file carries it too. On
    /// Windows the file takes the data directory's owner-scoped ACLs. The
    /// directory itself gets mode `0700` on Unix.
    ///
    /// Any failure along the way is [`IpcError::RemoteFileWrite`].
    pub fn write(&self, path: &Path) -> Result<(), IpcError> {
        let write_failed = |detail: String| IpcError::RemoteFileWrite {
            file: RemoteFile::SavedServers,
            path: path.display().to_string(),
            detail,
        };
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent).map_err(|error| write_failed(error.to_string()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
                    .map_err(|error| write_failed(error.to_string()))?;
            }
        }
        #[cfg(unix)]
        if path.exists() {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                .map_err(|error| write_failed(error.to_string()))?;
        }
        let data = serde_json::to_vec(self).map_err(|error| write_failed(error.to_string()))?;
        koshi_storage::atomic::write_atomic(path, &data)
            .map_err(|error| write_failed(error.to_string()))
    }

    /// The server `arg` names: the record whose name is `arg`, or when no
    /// name matches, the record whose address is `arg`.
    #[must_use]
    pub fn find(&self, arg: &str) -> Option<&SavedServer> {
        self.index_of(arg).map(|index| &self.records[index])
    }

    /// Save `server`, taking the place of whatever record already holds that
    /// address.
    ///
    /// The store is not written; the caller does that.
    pub fn save(&mut self, server: SavedServer) {
        self.records
            .retain(|record| record.address != server.address);
        self.records.push(server);
    }

    /// Drop the server `arg` names, returning its address, or `None` when no
    /// record matches.
    ///
    /// The store is not written; the caller does that.
    pub fn forget(&mut self, arg: &str) -> Option<String> {
        let index = self.index_of(arg)?;
        Some(self.records.remove(index).address)
    }

    /// Put `secret` on the server `arg` names, returning its address, or
    /// `None` when no record matches.
    ///
    /// The store is not written; the caller does that.
    pub fn set_secret(&mut self, arg: &str, secret: ConnectionToken) -> Option<String> {
        let index = self.index_of(arg)?;
        self.records[index].secret = secret;
        Some(self.records[index].address.clone())
    }

    /// Stamp the last-used time of the server `arg` names with `now`. No
    /// record matching `arg` changes nothing.
    ///
    /// The store is not written; the caller does that.
    pub fn touch(&mut self, arg: &str, now: SystemTime) {
        if let Some(index) = self.index_of(arg) {
            self.records[index].last_used_at = Some(now);
        }
    }

    /// Where in `records` the server `arg` names sits: the record whose name
    /// is `arg`, or when no name matches, the record whose address is `arg`.
    fn index_of(&self, arg: &str) -> Option<usize> {
        self.records
            .iter()
            .position(|record| record.name.as_deref() == Some(arg))
            .or_else(|| self.records.iter().position(|record| record.address == arg))
    }
}

/// Where the saved-server store lives: `remote/servers` under `data_dir`.
///
/// Callers resolve `data_dir` through `koshi_paths::data_dir()`.
#[must_use]
pub fn store_path(data_dir: &Path) -> PathBuf {
    data_dir.join("remote").join("servers")
}

#[cfg(test)]
mod tests;
